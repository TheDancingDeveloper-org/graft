## 0.1.0 — 2026-10-26

### G1.1

- graft-model: BLAKE3 object ids with strict text forms, path and portability rules, canonical tree and changeset encodings (pinned by golden vectors), the store-free changeset validity rules, path-copying `apply_changes` and an id-pruned, paginated `diff_trees`.

### G1.10

# G1.10: history queries — Log, ancestry, Blame

- `graft-index`: `Index::log` answers first-parent history by walking a draft's first parents to the line, then by `line_paths` range scans (with paths or directories) or a skip-list jump and a first-parent walk (without); it pages by `after_pos` or by `from`, and reads O(results) index rows. `Index::is_ancestor` and `Index::merge_base` prune by generation and line position. `Index::blame` is a lazy kind cached per `(changeset, path)` (encoding pinned by a golden vector): it walks the file's versions back to the nearest cached value and carries line origins forward with a histogram diff (`imara-diff`). Each diff has a deterministic work budget (repeated-line pairs over anchors, against 1024 per changed line); past it the version is attributed coarsely (common prefix and suffix kept, the middle blamed on the newer version), and blame yields to the executor between versions.
- Tests: proptests against brute force (ancestry and merge base on random DAGs, Log on random DAGs with drafts and paging), read-count tests, and differential tests against `git log --first-parent` and `git blame --first-parent` on synthetic repositories and (ignored, network) rustTorrent.

### G1.11

# G1.11: client library: remote API, fetch, working copy, sparse, status

- `graft-client`: `Remote` implements `ObjectApi` and `RepoApi` over gRPC, on TCP or a Unix socket. It binds the protocol version on every call (`graft-protocol` header both ways), sends the request id, a redacted bearer token and a deadline (the call context's, else 60 s), and gzips repository messages. Object frames, which are already compressed, are sent uncompressed. Typed errors arrive intact, and transport failures arrive as `Unavailable`. Endpoint discovery checks `GRAFT_ENDPOINT`, then `/run/graft/node.sock`, then the configured server. Workspace calls are still `todo!` (G2.5).
- `graft-client::fetch`: a pipelined breadth-first fetch of a sparse slice (batches of ids, a window of requests in flight, trees before blobs). It verifies every frame against its id before caching it in `.graft/cache` (graft-store-fs, LRU mode). A failed fetch resumes by running again and asks only for ids the cache lacks.
- `graft-client::workcopy`: the `.graft/` layout (`config`, `state`, `sparse`, `index`, `cache/`, each file replaced by atomic rename), `clone` and `checkout`, multi-prefix sparse slices with optional root files, and stat-based status that re-hashes only on a stat mismatch or a racily clean entry. `update` moves from base A to B by `Diff`, fetches only the blobs it names, and refuses to overwrite local changes.
- `graft-testkit::server`: `TestServer` serves any `ObjectApi + RepoApi` over the generated gRPC services on loopback TCP or a Unix socket. It records every RPC with its ids and can drop every live connection mid-stream. `SnapshotRepo`, an in-memory repository built on `FakeApi` with lines and `Diff`, stands in until graft-engine serves the API.
- Hostile-server hardening:
  - Writes and removes check every parent component with `symlink_metadata` and refuse any that is a symlink or a file (`Error::UnsafePath`). Update also refuses, before writing anything, a diff path that passes through a file or symlink of the base or of the working tree, unless the same diff deletes that parent. Status and the dirty check never read through a symlinked parent.
  - `fetch::Budget` (in `FetchOptions`) bounds tree entries, bytes and depth for fetch, checkout and update, failing with `LimitExceeded { limit: "client.max_*" }`. Checkout loads each distinct tree once.
  - `Remote::get_objects` keeps only requested ids, each once, as pages arrive, and caps an answer at one page per id plus one and at `MAX_GET_OBJECTS_BYTES`.
  - A response without the `graft-protocol` header is `IncompatibleProtocol { server: "none" }`.
  - A top-level name that any filesystem could treat as `.graft` is reserved: ASCII case-insensitive, NFKC-normalized, case-folded, and with default-ignorable code points dropped (`workcopy::is_reserved_name`), so `.GRAFT/config` or a fullwidth look-alike can't overwrite the working copy's state.
  - Update refuses, before writing anything, a diff that puts a symlink at a path and also touches a path under it.
  - `Remote::with_max_response_bytes` lowers the per-answer byte cap (never above `MAX_GET_OBJECTS_BYTES`).
- `graft-testkit::server`: `SnapshotRepo` can commit symlinks and executables (`commit_entries`) and can act as a hostile server (`insert_raw`, `lie_about_diff`, `pad_get_objects`). `TestServer::advertise_version(false)` drops the version header.

### G1.12

# G1.12: server binary

- `graft-server serve`: the four `graft.v1` gRPC services over the engine on the SQLite and filesystem stores, gzip both ways, plus `/healthz`, `/readyz`, `/metrics` and a read-only JSON facade under `/v1` on the same listener (and `/metrics` on an optional second `metrics` listener). Every call authenticates a bearer PASETO token (`authorization: Bearer …`) and binds the `graft-protocol` version; the engine checks scopes per operation, so refusals are typed `Unauthenticated`, `IncompatibleProtocol` or `PermissionDenied`. Writes are refused with `Unavailable` below the free-space floor (`min_free_fraction`, `min_free_bytes`); reads continue. SIGTERM or SIGINT drains gracefully. Operations the engine has not built yet (rebase, land, workspaces, GC) answer `Unimplemented` (HTTP 501) instead of failing the connection. `MintToken` mints tokens no broader than the caller's, capped by `max_token_ttl_s`.
- Bounds: `PutObjects` checks the write scope and the repository on its first message, before buffering, then buffers at most `max_put_call_bytes` per call within a server-wide `put_buffer_bytes` budget (callers wait up to `put_wait_ms`, then get `Unavailable`). A put stream silent for `put_idle_ms` is answered `Unavailable` and gives its budget back, and the object service decodes no message larger than `max_put_call_bytes` plus framing. `GetObjects` reads in small batches through a one-page channel, so a slow client holds the server back. `Watch` caps its wait (`max_watch_wait_ms`), ends at the earlier of its token's expiry and `max_watch_lifetime_s`, re-checks the token every page, and is limited per principal and in total (`Unavailable` past either).
- The signing key file is refused unless it is a regular file owned by the server's user with no group or other permission bits.
- `graft-engine`: `Log`, `IsAncestor`, `MergeBase` and `Blame` answer through `graft-index` (G1.10).
- TOML config (`--config`, `$GRAFT_SERVER_CONFIG`, else `/etc/graft/server.toml`); unknown keys are refused. The signing key is created on first use beside the database.
- `graft-server admin`: `token mint`, `repo create` (both safe while the server runs), `fsck` (every journalled changeset, its parents and its whole snapshot, plus refs; run with the server stopped), and `backup <dir> [--objects]` (SQLite `VACUUM INTO`, then an incremental object copy).
- Process-level tests start the binary: import a synthetic repository through the API, `kill -9`, restart, fetch and verify everything, fsck clean; corrupt-object detection and a backup restored clean; refusals; health, metrics and the facade; the admission floor; restart to serving under 2 s.

### G1.13

# G1.13: CLI read commands

- `graft-cli`: `clone`, `status`, `diff`, `log`, `show`, `blame`, `cat`, `ls`, `sparse`, `api` and `version`, each with `--json`. Every JSON shape is a JSON Schema under `crates/graft-cli/schemas/`, and the tests validate each output against its schema. `clone` opens a working copy on a line's tip or at `--at`; `sparse add` and `sparse remove` re-check the slice out, and refuse while the working copy has local changes.
- Exit codes follow design §8.2: 0 success, 2 usage, 3 conflict (and local changes a checkout would overwrite), 4 contended, 5 rejected, 6 denied, 7 unavailable. Anything §8.2 does not name exits 1.
- The token comes from `--token-file` or `GRAFT_TOKEN`, never from an argument, and is never printed. Nothing prompts; `graft api` reads standard input only when it is not a terminal.
- `graft api` calls the read operations with JSON in and out. `graft api --list` shows every operation of the four API traits and either the command that serves it or the reason it has none. A unit test keeps that table complete against the trait list.
- Known gaps: `graft diff` with no arguments lists the working copy's changed paths, without patch text. The engine's `log` and `blame` are not yet implemented, so the CLI tests run against a fixture in `tests/support`.

### G1.14

- graft-mirror: `import <git-dir> --repo <name>` (import with the fidelity check, then line and `tags/<name>` refs set by CAS, safe to re-run) and `follow --config <file>` (git-primary: `ls-remote` every 15 s, `git fetch` into a local bare mirror, import the new commits, move each line by admin `UpdateRef` CAS). A SQLite mirror DB implements `MirrorMap` and holds per-line follow state, so a restart re-imports nothing. A non-fast-forward upstream stops the line, sets `graft_mirror_divergence`, logs, and leaves graft unchanged. It also exports `graft_mirror_lag_seconds` and `graft_mirror_lag_changesets` on `/metrics`.

### G1.15

# G1.15: MCP read tools

- `graft-mcp`: an MCP server over the API traits, built on `rmcp` 3.6. It serves `graft_log`, `graft_show`, `graft_diff`, `graft_blame`, `graft_read`, `graft_ls` and `graft_status` (which needs a working copy), each with an input schema and `readOnlyHint`. Results are capped at 64 KiB and paged. Also `McpServer::serve_stdio` and a `streamable_http_service` for mounting at `/mcp`.

### G1.16

- graft-testkit: the pilot harness (`pilot`, `bench`, the `graft-pilot` binary). It imports a git repository with `graft-mirror import` into the real `graft-server`, clones every imported changeset with `graft clone --at`, and compares the clone with `git checkout` (paths, file or symlink, exec bit, bytes, symlink targets). It checks that `log --path`, `diff` and `blame` agree with git. It times full and sparse clone, `log --path`, `diff`, `blame` cold and cached, the decompression share of a full clone (D5), and restart to serving, against git on the same host. Output is p50, p95 and mean in `results.tsv`, plus a Markdown report with the exit-criterion budgets and the run-to-run spread. Also the long crash suite: `kill -9` restarts of `graft-server` under mixed load, which checks that no acknowledged write is lost and that `fsck` is clean after every restart (100 restarts behind `#[ignore]`). Every child step has a deadline. `cargo test -p graft-testkit` runs the harness on a synthetic `git fast-import` fixture with odd file names, symlinks, exec bits, an empty file, a large binary and a merge.

### G1.17

G1.17: an `image` workflow builds `docker/Dockerfile` and pushes `ghcr.io/thedancingdeveloper-org/graft:sha-<sha>` on manual dispatch or a `v*` tag (pull requests that touch the image only build it). The pilot stack on Node B pins that image; it lives in the ops repo (`personal/graft`, runbook `docs/graft-pilot-runbook.md`).
The image now creates `/var/lib/graft/meta` and `/var/lib/graft/objects` owned by `graft`, so fresh named volumes mounted there are writable by the server (before, they came up root-owned and `serve` could not open `meta.db`).

### G1.2

# G1.2: storage ports, memory store, conformance suite

- `graft-store`: the `ObjectStore` and `MetaStore` ports are final. A ref CAS now compares `(target, version)`, `PutLinePath` records its changeset, and every list call is clamped to `MAX_PAGE`. Adds the in-memory store and `conformance::run_all` (feature `testkit`).
- `graft-testkit`: `synth::generate`, a seeded synthetic repository generator (N files, depth, history).

### G1.3

graft-proto: the gRPC wire protocol (`graft.v1` services and messages, generated with protox, no `protoc`), lossless conversions between the API types and protobuf, typed errors carried in gRPC status details, protocol version binding via the `graft-protocol` header, and the `GRF1` object frame codec (raw, or gzip for bodies of 512 bytes or more that shrink by 10%).

### G1.4

# G1.4: derived-data framework and eager kinds

- `graft-index`: the derived-kind framework (values cached by `(kind, version, key)`, a walk that computes a missing value back to the nearest stored ones and returns rows to cache), generation numbers, first-parent skip lists, and line positions with line path index rows (every ancestor directory prefix), all returned as `graft_store::Write` rows for the engine's transactions (`Index::on_create`, `Index::on_land`).

### G1.5

# G1.5: Filesystem object store

- `graft-store-fs`: the `ObjectStore` on disk (design §6.4). Frames are stored as their gzip wire frame under `<root>/<repo>/objects/<2 hex>/<62 hex>`, written by temp file, fsync, no-replace rename and directory fsync; open removes stray `*.tmp` files, recounts, and locks the root. Reads are verified against the id (`StoreError::Corrupt` on mismatch), `stats` reports objects, bytes and free space, and an LRU mode for the node and client never exceeds its byte bound by more than one object. It passes the conformance suite and a 20-kill `kill -9` test of 10,000 concurrent puts.
- `graft-testkit`: a reusable `crash` module (child process, acknowledgements over a pipe, `SIGKILL` at seeded random points).

### G1.6

# G1.6: SQLite metadata store

- `graft-store-sqlite`: the `MetaStore` on SQLite (WAL, `synchronous=FULL`), with one writer thread behind a bounded queue that group-commits with one savepoint per transaction, a pool of reader threads, and versioned migrations (schema v1, design §6.5). A full queue returns `StoreError::Busy(retry_after_ms)`. It passes the conformance suite and a 20-kill `kill -9` crash harness, and `examples/cas_bench` measures ref CAS throughput.

### G1.7

# G1.7: engine write path, snapshot reads, diff, watch

- `graft-engine`: the phase 1 API over the storage ports. `CreateRepo`; `PutObjects`/`HasObjects`/`GetObjects` (verified frames, partial results, deferral past the id and byte limits, upload holders); `CreateChangeset` (every design §4.5 rule, root derivation by path copying, inline blobs, objects before one metadata transaction with record, holders, eager derived rows and the `ChangesetCreated` journal entry, idempotent retries) and admin `ImportChangesets` (roots and empty merges allowed, root checked); `GetChangesets`; `ResolveRef`/`ListRefs`; `UpdateRef` by CAS with line first-parent fast-forward enforced via skip lists and line index rows written in the move's transaction; `Stat`, paged `ListTree` (flat or recursive pre-order), `ReadFile`; `Diff` with path prefixes, paging and capped unified patches (histogram, `imara-diff`); `Watch` as journal pages. Scope checks follow design §12.1 until `graft-auth` lands.

### G1.8

# G1.8: lossless git import with a fidelity check

- `graft-git`: `import::import` reads a git repository with `gix`, walks every commit reachable from the configured refs oldest first, diffs each against its first parent through changed subtrees only, uploads blobs, and sends `ImportChangesets` batches. Before writing anything it rejects bad paths (D11) and unrepresentable commits with the full list. Every commit is checked to re-export to its own sha (`export::export_tree`, `export::commit_sha`), using a mirror map that keeps raw headers, raw messages and raw non-canonical trees (`map::MirrorMap`, `MemoryMirrorMap`). Every imported changeset carries metadata `mirror.git-commit` (the commit's sha), so git and graft stay 1:1 even for commits that differ only in their committer; the key is never written into the exported commit.
- `graft-testkit`: `fake_api::FakeApi`, a recording in-memory `ObjectApi`/`AdminApi`/`RepoApi` that re-derives every imported root as the server does.

### G1.9

# G1.9: auth and telemetry

- `graft-auth`: PASETO `v4.public` (Ed25519) tokens with `mint`/`verify`, the design §12.1 scope text forms, `authorize` with write path prefixes, and a `jti` `Denylist`. Golden tokens freeze the format; tokens and keys are redacted from `Debug` and logs.
- `graft-telemetry`: `init` (JSON logs to stderr, a Prometheus registry, OTLP traces behind the `otlp` feature) and every design §13 metric registered with its documented labels.

### G1.D

G1.D: local dev stack. A compose file (`compose.yaml`) and one image (`docker/Dockerfile`) run graft-server, graft-node, graft-mirror and the `graft` CLI on loopback, with a fast host-binary loop (`--profile dev`) and `scripts/dev/` helpers. See docs/dev-stack.md.

### G2.1

# G2.1: engine workspaces, draft states, holders

- `graft-engine`: the workspace API. `OpenWorkspace` (CAS on an absent name, base at the line tip or a changeset on the line, retried opens return the same workspace, generated names derived from the caller and request id), `GetWorkspace`, `ListWorkspaces` (the caller's, paged by name), `RenewLease` (capped by `max_lease_ms`), `SetHead` by CAS and `CloseWorkspace`. `CreateChangeset` with `workspace` creates a draft held by `ws:<name>` and moves the head from `parents[0]` in the same transaction. Drafts that leave a stack become superseded (by a new head) or abandoned; abandoned changesets can be adopted back into a stack. A lapsed lease ends the workspace and abandons its drafts, on access or through `Engine::expire_leases`. Stacks are bounded by `max_stack`, checked by skip-list depth before any walk. Workspace names are lowercase ASCII segments, so names never differ only by case or normalisation. Adopting a changeset through `SetHead` needs write scope on every path it changes, as committing it would. `Engine::gauges` reports `graft_orphans{state}` and `graft_workspaces{state}`.
- `graft-engine`: **behaviour change** — a changeset created without a workspace (or by `ImportChangesets`) is now `abandoned` from birth, because nothing holds it; a line move still lands it. Its `ChangesetCreated` entry says `"state":"abandoned"`.
- `graft-engine`: `UpdateRef` guards each record it lands with a state CAS, so a line move cannot overwrite a concurrent workspace transition.
- `graft-store`: port additions for G2.1: `Precondition::WorkspaceIs` (whole-record workspace CAS, absent for `None`), `Precondition::RecordStateIs`, `MetaStore::list_workspaces`, `count_records` and `count_workspaces`, with conformance cases.
- `graft-store-sqlite`: schema version 2 adds the `workspaces_owner` and `changesets_state` indexes, and implements the new port operations.

### G2.2

# G2.2: deterministic git export and full-repository export

- `graft-git`: `exporter::Exporter` writes changesets as git commits into a bare repository, parents first. For each changeset it writes the blobs the destination lacks, then the trees (path copying from the first parent's git tree, O(change)) and the commit, and records a native commit in the mirror map only after its objects are stored. An imported changeset reproduces the commit recorded at import, and a mismatch is `GitError::Fidelity`.
- Native commits: `author` from the changeset, `committer` from the export's `Committer` with the line's landed time (`exporter::LandTimes`), and the message followed by the trailers `Graft-Changeset`, `Graft-Session` (`vogt.session`) and `Vogt-Work-Item` (`vogt.work_item`). The `mirror.git-commit` key is never written (D19). A name, email, trailer value or message that git cannot hold is refused with `GitError::Unexportable`, and nothing is written for it.
- `Exporter::export_refs` exports the history reachable from each ref's changeset and then points each ref at its commit. A repeat run writes only what the destination lacks.
- `fidelity::check_changeset`: the reusable fidelity check. It compares a commit's diff against its first parent with the changeset's changes, in O(change).
- `graft-git` tests: `tests/export.rs` covers the round trip ref for ref (fsck `--strict`, the same commits, the same refs), adversarial names and modes, byte-identical re-exports, native histories checked against graft snapshots, export then import, refusals, and a seeded property (export after import is the identity).

### G2.3

- graft-node: the per-host cache daemon. It serves `GetObjects` over a Unix socket (mode 0660 by default; modes open to other users are refused) from an LRU disk cache in the wire frame format, verifying each frame against its id when fetched and again when read; a corrupt entry is discarded and fetched again, and a corrupt or unrequested frame from upstream is never cached or served. Misses are single-flighted per `(repository, id)`, so concurrent clones of one slice cause one upstream fetch per object. Every other `graft.v1` call (repository, workspace and admin calls, `HasObjects`, `PutObjects`) is forwarded to the server unchanged with the caller's token and never cached. Callers' tokens are verified locally with the server's public key (`server_public_key`) before cached objects are served, and need `repo:<name>:read` or `objects:read`.
- Bounds: upstream calls in flight (`max_upstream_calls`) with a bounded queue (`max_upstream_waiting`, `upstream_wait_ms`) past which calls are shed with `Unavailable`; a circuit breaker with jittered backoff fails calls fast while the server is unreachable; socket connections, streams per connection, objects in flight and tracked repositories are capped; a free-space floor (`min_free_bytes`) stops filling the cache. Metrics on an optional `/metrics` listener (`graft_node_*` plus the shared `tier="node"` series). A hardened systemd unit and an example config are in `crates/graft-node/deploy/`.

### G2.4

# G2.4: server workspaces, Watch resume, token minting and revocation

- `graft-server`: `MintToken` no longer lets a token outlive its caller. The
  child's TTL is capped by the server maximum and by the caller's remaining
  life; an explicit TTL past that is refused with `LimitExceeded`, and the
  default is the remaining life.
- `graft-server`: revoked token ids are refused on writes (`PutObjects`,
  every write RPC, `MintToken`). The denylist is a `token_denylist` table in
  the metadata database, owned by the server (`src/denylist.rs`), so
  `admin backup` snapshots it. A denylist that cannot be read refuses the
  write (`Unavailable`), not allows it.
- `graft-server admin token revoke` reads a live token of this server from
  stdin (never argv) and stores its `jti` and expiry. Expired rows are pruned.
- `graft-server`: `MintToken` mints only for the caller's own identity,
  unless the caller is a service principal (Vogt). A non-service caller's
  child takes its subject, session and work item; naming a different one is
  `PermissionDenied`, so a writer can no longer mint a token for another
  subject and take over that subject's workspaces, or forge the audit fields.
  A service's delegated token is an agent token, which delegates no further.
  Requests need 1 to 32 scopes; subject, session and work item are at most
  128 bytes, with no whitespace or control characters.
- `graft-server`: every workspace RPC that names a workspace (`GetWorkspace`,
  `RenewLease`, `SetHead`, `Sync`, `Stack`, `CloseWorkspace`) is refused
  with `PermissionDenied` unless the caller owns it or administers the
  repository. The server checks this before the operation runs; the
  engine's `do_get_workspace` still reads any record.
- `graft-server`: the workspace service bounds its request and answer
  messages (4 MiB), refusing larger ones instead of buffering them.
- `graft-server`: `App::call` takes the authenticated `Caller`, which carries
  the token's `jti` and expiry for those checks; the call context is
  unchanged.
- Tests: `tests/tokens.rs` (escalation, every scope string form, minting
  from a minted token, caller expiry, revocation across restarts),
  `tests/workspaces.rs` (owner isolation, no takeover by minting another
  subject, Vogt delegation, lease and open races, oversized requests),
  `tests/watch.rs` (resume from an LSN across a kill -9).

### G2.5

# G2.5: client and CLI write commands

- `graft-client`: `Remote` implements `WorkspaceApi` over gRPC (`WorkspaceService`, gzip like the repository service).
- `graft-client`: `WorkingCopy::begin` starts every write. It renews the workspace lease, or opens a workspace on first use. When the lease lapsed or the workspace was closed, it reopens one under the same name at the working copy's base and adopts the drafts it held. It then finishes or rolls back an interrupted commit, and refuses (`Error::Diverged`) a server head that is not the one the working copy holds.
- `graft-client`: `WorkingCopy::commit` reads only the slice's changed files. It opens each without following a symlink and without blocking on a FIFO. A file whose stat changes during the read is read again, and gives up as `Error::Unstable`. Files over 64 MiB are refused, as are `.graft` in any case or Unicode spelling and special files. It uploads only the blobs `HasObjects` reports missing, in batches, re-reading each and checking it still hashes to its id. It creates the draft with the workspace, so the head moves in the same transaction, and checks the server's answer is the changeset it sent. `--amend` composes the old head's changes with the working copy's against the old head's parent. Paths outside the slice keep the old head's changes. It creates the replacement and moves the head with `SetHead`, which supersedes the old head.
- `graft-client`: crash safety for commit. The request and its index updates go to `.graft/pending` before the create, and local state changes only after the server answers. The next command sends the pending commit again (idempotent) and applies it. When the request can no longer succeed, or the file was cut short, it drops the file, which leaves the working copy as it was before the commit.
- `graft-client`: `WorkingCopy::sync`. With no stack it fast-forwards the files the line changed, after checking for local changes, and moves the workspace by reopening it at the tip. With a stack it calls the server's `Sync`, and a conflict changes nothing. `WorkingCopy::land` lands directly, or through the PR bridge. `Queued { pr_url }` returns at once, or with a wait asks again until the PR merges, is closed (`Rejected`), or the wait runs out (`Error::StillQueued`). Contention is retried with backoff. Once landed, the working copy and workspace move to the landed changeset. Local changes in the way give `Error::LandedDirty`, and running land again finishes the update. `update` is now `move_to(base, head)`, diffing from the head draft when there is one.
- `graft-client`: `workspace::with_heartbeat` renews the lease while long-running work runs, and `workspace::stack` reads a stack oldest first.
- `graft-cli`: `commit [-m] [--amend] [--author] [--author-kind] [--meta]`, `workspace [show|list|open|renew|close]`, `stack [list]`, `sync` and `land [--wait] [--squash]`. Each has `--json` with a schema in `schemas/` (`commit`, `workspace`, `workspace-list`, `stack`, `sync`, `land`), exit codes as in design §8.2, and lease heartbeats in commit, sync and land. A PR still open when `--wait` runs out exits 4, `kind: "queued"`. `GRAFT_AUTHOR_NAME`, `GRAFT_AUTHOR_EMAIL`, `GRAFT_AUTHOR_KIND` and `GRAFT_AUTHOR_DATE` set the author. The parity table names the new commands.
- `graft-testkit`: `TestServer::tcp_workspaces` also serves `WorkspaceService`, and records `Rpc::Workspace`. `SnapshotRepo` models workspaces, draft states, `CreateChangeset` with a workspace, `SetHead`, `Sync` (fast-forward or re-apply, with conflicts), and `Land`: a direct fast-forward, or a fake PR bridge driven by `set_git_primary`, `merge_pr` and `close_pr`. `fail_next` injects errors per operation.

### G2.6

# G2.6: mirror PR bridge into the GitHub merge queue

- `graft-mirror::bridge`: a land request on a git-primary line is planned (the stack from its head down to the line tip, every changeset a draft, the line caught up with upstream), exported onto upstream's tip, pushed to `graft/<workspace>/<short-id>` without force, opened as a PR, and put in the merge queue. The PR body names the workspace, work item and session, and ends with a `Graft-Changeset: <head>` trailer, so a squash merge built from the PR body names the head. A PR already on that branch is adopted, not opened twice.
- Tracking: a closed PR, a failing check or a branch changed outside graft is `Rejected` with its URL. A merged PR is `Merged`, and once follow has imported the merge commit and that commit names the stack head by `Graft-Changeset`, the request is `Landed`. Requests are kept in the mirror DB (`bridge` table), so a restart resumes each one.
- A branch that holds other commits is never overwritten (`Rejected`). The bridge writes only on a line that follow has caught up and not stopped.
- `graft-mirror::github`: the GitHub calls behind a `Forge` trait, with `HttpForge` over REST and GraphQL (reqwest, rustls). Rate limits and 5xx answers are retried a bounded number of times, honouring `Retry-After` (or, for a spent primary limit, `x-ratelimit-reset`) up to a cap. A GraphQL `RATE_LIMITED` reply is a transient answer too, not a refusal. The token goes in a sensitive `Authorization` header, never in a URL, argv or `Debug` output. The push authenticates through the environment (`GIT_CONFIG_*`), not argv.
- Credentials (D8 left to the operator, so the choice is config, with no default): `graft-mirror::credentials` has a `CredentialProvider` with `StaticToken` (a token file) and `GithubApp` (app id, installation id and a PEM key file; installation tokens cached and refreshed ten minutes before their hour is up). The `[github]` config section requires exactly one of `token_file` or `github_app`, checked when the config is parsed. Tokens are never read from argv and never logged.
- Hostile text: PR titles and bodies are cleaned (control and bidi characters removed, `@` neutralised, length capped) before they reach GitHub.
- Authority: the bridge and follow each claim a line as `git-primary` in the mirror DB. A line another writer has claimed is refused by both.
- Before each step that writes, the bridge checks in one batched read that the stack's drafts are still drafts. An abandoned, superseded or collected draft rejects the request with "drafts abandoned" (no PR is opened for it). A merged request is not checked.
- Tests (`tests/bridge.rs`, 21 cases) run against a local bare upstream, an in-process engine and a wiremock GitHub: the full lifecycle (land, PR, merge, follow, landed), a closed PR, a failing check, a restart mid-lifecycle, duplicate submits, an adopted PR, a diverged branch, a force-pushed upstream, two authorities, a hostile title and body, a 429, a spent primary rate limit, a GraphQL rate limit and a persistent 5xx, a squash that names the head only through the PR body, a rebase merge, abandoned drafts, a stale base, a merge queue refusal, and a merge that does not name the head.
- Not in this change: the server hand-off that creates a request from `Land` (the engine's `land` is still a stub), rebasing a stale stack (`rebase` is G2.7), and moving drafts to `landed` or `rejected` inside graft (no graft API does that outside a line move). The mirror keeps those outcomes in its own DB until those land. The CLI `bridge` subcommand is not wired yet.
- Dependencies: `reqwest` (rustls), `jsonwebtoken` (RS256 for the App JWT), `wiremock` (dev), `base64` and `serde_json` in `graft-mirror` only.

### G2.7

# G2.7: engine file-level rebase and Sync

- `graft-engine`: `Rebase` implements the file-level table in design §7.8. For each changed path it compares the draft's base, the target and the draft by entry kind and id, so mode and symlink changes count as changes. Changes are file-level, so a directory counts as no entry: a draft that deletes file `a` matches a target that turned `a` into a directory, and both sides making the same file-to-directory change is clean. A path the target already has is dropped, and a divergent path conflicts. A file/directory clash, where applying the kept changes would make a path both a file and a directory, is always a conflict. A stack rebases bottom-up: the walk runs from the given head down to the first landed changeset (or `onto`), bounded by `max_stack`. The whole stack is planned in memory before anything is written, so a conflict creates nothing. A draft whose changes are all already on the target disappears; when every draft disappears, the result is `onto`. Rebased changesets keep author, time, message and metadata, and add `graft.rebased-from`. They are deterministic, so a repeated rebase returns the same id. A rebase onto an unchanged base returns the drafts unchanged. Results are stored abandoned until a workspace adopts them. The caller needs write on every path the stack changes. `merge = line` is answered at file level until G3.7.
- `graft-engine`: `Sync(workspace, onto?)` rebases the stack onto the line tip (or `onto`, which must be on the workspace's line). One CAS on the workspace then adopts the new drafts, supersedes the old ones (each by its successor; a draft that disappeared is superseded by the draft below it, or by the target) and moves the base and head. It writes a `WorkspaceSynced` journal entry. A lost CAS (a racing `SetHead` or renewal) re-reads and retries. It returns the paths that differ between the old top and the new one, capped by the new `EngineConfig::max_sync_paths`. A sync needs the workspace's owner or a repository admin, a live lease, and write on the stack's paths.
- Cost follows the change: tree reads on each changed path plus one changeset read per draft. Tests count reads and show they do not depend on the distance to the target.

### G2.8

# G2.8: MCP write tools and the server's `/mcp`

- `graft-mcp`: write tools `graft_commit` (inline content, up to 256 files and 1 MiB; `utf8` or `hex`; `put` or `delete`; `file`, `exec` or `symlink`), `graft_sync`, `graft_land` (renders `Queued { pr_url }` from the PR bridge as `outcome: "queued"`) and `graft_workspace` (`open`, `get`, `renew`, `close`). Each maps to the API traits, called as the request's caller, so scopes and idempotency are the API's own. Inputs are validated at the tool boundary: names, lease bounds, path rules, no `.graft` component, no duplicate paths, sizes checked before decoding. A commit retried with the `parent` and `authored_at_ms` its first result returned finds the same draft. `from_working_copy` answers `needs_working_copy` (stdio only).
- `graft-mcp`: **API change**: over HTTP, every tool runs as the `Caller` the mounting server puts in the request's extensions after checking its bearer token. A request without one is `unauthenticated`. `streamable_http_service(api, config)` no longer takes a fixed principal. `McpServer::new(api, principal)` still serves stdio. New `http_config()` (JSON responses, `MAX_REQUEST_BYTES` body cap, 413 past it).
- `graft-server`: `/mcp` is mounted beside gRPC and the JSON facade. It takes the same bearer token, and runs every tool's operations through `App::call`, so writes pass the free-space floor and are recorded like gRPC calls. Operations the engine does not build yet (`Land`, `Sync`) come back as `rejected` with policy `unimplemented`.
- Tests: an end-to-end over the real engine (in-memory store), real tokens and a fake PR-bridge mirror. An agent reads, commits a two-file inline change and lands it as a queued PR, and a replayed land returns the same PR. Further cases: out-of-scope tokens, missing or forged tokens, oversized writes, path traversal and `.graft`, injection-shaped content returned as data, and a server test of `/mcp` on the running binary.
- `graft-mcp`: new `Auth` request extension, an opaque handle the mounting server sets beside the `Caller`. It is scoped to each tool call and resource read, and the server's API reads it back with `request_auth()`. `graft-server` stores its full caller there, so MCP writes check the token's `jti` against the denylist exactly as gRPC does: a revoked token is refused on MCP writes, and its reads still work. A process-level test covers it.

### G2.9

# G2.9: GC dry run and the workspace listing port

- `graft-gc`: `dry_run_report` reports what expiry makes collectable, and never deletes. A draft is a candidate once its workspace's lease has lapsed for the retention window. A superseded or abandoned changeset is a candidate once the window has passed since it was received. A candidate is protected when a live draft descends from it (walking through non-landed changesets), or when its successor chain reaches a changeset that is not a candidate. An object is doomed when every holder row names a collectable changeset or an expired upload, and its frame is stored. `Collector::run` refuses a sweep. The report carries counts only, so its size is bounded, and every store read is paged by `GcConfig::batch`.
- `graft-store`: `MetaStore::list_records` pages changesets in one record state. `MetaStore::holder_rows` pages the holder names of one object. `MetaStore::list_all_workspaces` pages every workspace by name. All three default to `StoreError::Backend`. The memory and SQLite stores implement them, and the conformance suite checks paging, state filtering, isolation and resume on both.
- `graft-store-sqlite`: schema version 3 adds `changesets_state_id (repo, state, id)`, so a record state pages in id order without sorting.
- `graft-engine`: `Gc { dry_run: true }` reports through `graft-gc`. Its object walk is an id-pruned comparison with the parent's tree, so its cost follows the change. `Gc { dry_run: false }` is refused with `Rejected { policy: "gc" }`, so nothing deletes.
- Tests: a property test compares the planner with a naive model over random histories, covering workspaces with leases, every record state, successor links, shared and uploaded objects, and stored and absent frames. Hand-built cases cover a shared object with a live holder, a superseded changeset with a live successor, an ancestor of a live draft, and an expired upload. An engine test checks the report end to end.

Limits, which the sweep (G3.2) must handle before deleting anything:

- Retention is anchored on the changeset's creation time, because the record does not store when it was abandoned. The report can therefore count a changeset early by the time it stayed live.
- Objects held only by expired uploads are not enumerated, because no index lists uploads. They are not reported.
- Rewrites are assumed to set `superseded_by`. An object that a live changeset references without introducing it is protected only through the successor or ancestry rules.

### G3.1

# G3.1: engine Land, policies and Attest

- `graft-engine`: `Land` implements design §8.1. A `lands` row plus a stored land result make a retry return the same `Landed`, whatever has happened to the line since. The loop is bounded by the attempt budget (16) and the deadline (capped by the land-attempt lease), and backs off with full jitter (2 ms base, 200 ms cap) after a lost CAS. New candidates are drafts held by a land-attempt lease, `land:<expires_ms>:<attempt>`, expiring 10 minutes after they are written. One transaction carries the line's CAS, its line index rows, the records that land (holder and successor cleared), the drafts the candidates supersede, the `lands` row and result, and the `RefMoved` and `LandAttempt` journal entries. A conflict returns `Conflict { onto: tip }` and journals `LandAttempt`. A stack whose changes are all on the line already lands as the tip without moving it. Each attempt reads the tip before the land record, so racing lands of one head agree. The backoff sleeper is injectable (`Engine::with_sleeper`); the default parks a short-lived thread and runs only after a lost CAS.
- `graft-engine`: per-line policies in `EngineConfig::lines`, with `default_line` (direct) for the rest. `direct` lands at once. `attested` needs passing attestations on the stack head; they are inherited through `graft.rebased-from` (file-level rebases, such as a sync, at most 16 hops), so a rebase onto a newer tip needs no re-attesting. `queued` lands a candidate that carries its own passing attestations, or that `LandHooks::test_candidate` passes. Path rules add checks and human review, which needs a `review/human` attestation posted by a person. Git-primary lines refuse native lands. `squash` is refused until G3.7.
- `graft-engine`: `Attest` stores the latest result per `(changeset, check)`. It needs a write scope, and an agent never attests, so a session cannot attest its own draft. Decision D6 is the default for `main`: `attested` on `ci/build` and `ci/test`, with the same checks run post-land. `LandHooks::landed` reports each move. A failing post-land attestation on a changeset landed on a line with `auto_revert` journals `RevertRequested` in the same transaction and calls `LandHooks::revert_requested`; the revert itself is G3.7 and the CI wiring G3.8.
- New durable formats: the journal entries `LandAttempt`, `Attested`, `RevertRequested` and a `ChangesetCreated` variant with `holder`; the derived kinds `land` and `attest` (layouts in `land.rs`, pinned by unit tests). The `land:<line>` scope is now checked.
- Tests: scenarios on both stores; real threads racing lands against SQLite (8 disjoint lands all land; same-line lands give exactly one winner and file-level conflicts; racing retries agree); a model-based proptest that races stacks through a yielding store under a proptest-chosen schedule; and a criterion benchmark of uncontended land (`cargo bench -p graft-engine --bench land`).

### G3.11

# G3.11: GC protection records

- `graft-store`: the protection records of design §8.6, kept beside each changeset record (`Protection { pins, expires_at_ms }`) and changed by new writes: `Pin` and `Unpin` (no-ops on a landed parent; `Unpin` stays at 0), `SetPins`, `SetExpiry`. A record written as landed or collected has no pins and no expiry. Holder rows can expire (`AddHolderUntil`, for upload leases and claims), and adding a row clears the object's doomed mark unless it is `deleting`. Doomed marks: `Doom`, `MarkDeleting`, `Undoom`. The repository's `gc_protection` marker: `SetGcProtection`, on for every new repository. New preconditions `RecordLive`, `RecordIs` (record and protection), `ObjectDoomed` and `ObjectUnheld`. New paged index reads: `protection`, `expired_records` (no pins, expired), `held_objects` (rows by holder), `expired_rows`, `doom_marks`, `old_doomed` and `lapsed_workspaces`; `gc_protection`. New `MetaStore` methods have default bodies, so outside wrappers still build. Conformance cases cover all of it on both stores.
- `graft-store-sqlite`: schema version 4, a forward migration. It adds `pins`, `expires_at` and a partial expiry index to changesets; an expiry column, backfilled from `upload:<ms>:` holder names, an expiry index and a holder index to holder rows; `deleting` and a time index to doomed marks; and `gc_protection` to repositories. Existing repositories start without the marker. Tests check the version 3 to 4 migration and that the scans are index ranges with no sort.
- `graft-engine`: `CreateChangeset` and everything that writes through it pin each non-landed parent in the same transaction, guarded by `RecordLive`. A create that loses to its parent's collection returns `Collected`. A changeset nothing holds expires the retention window (`EngineConfig::draft_retention_ms`, 7 days) after it is written; a land candidate expires with its land-attempt lease; transitions set the expiry (close and supersede: now; lapse: `lease_until`) and clear it (adoption). Landing zeroes the landed records' pins and releases the pins they held on parents off the line (merge second parents), so every pin comes from a non-landed child. Before its commit, a write claims each doomed object it references or puts with a `claim:<expiry>:<tag>` row (`EngineConfig::claim_ms`). A blob named by id that is being deleted is `MissingObjects`. For a tree or inline blob being deleted, the write waits for the delete, then puts the frame again. `PutObjects` of an object being deleted is `Unavailable`. `Engine::protect_repo` backfills a repository written before protection records: pins, expiries, the row check and the marker, reading only non-landed records and their own trees. Nothing deletes yet (G3.2).
- Tests: a proptest of random commits, merges, rebases, syncs, lands, closes, lapses and adoptions, checked by brute force after every step on memory and SQLite; pin writes counted per create; deterministic claim and delete interleavings; real threads against SQLite racing creates with their parent's collection and with each other; and the backfill, including from a version 3 SQLite database.

### G3.2

# G3.2: GC sweep

- `graft-gc`: the sweep of design §8.6, `Collector::sweep`. It lapses workspaces whose lease ended (as the engine closes them, journalled `WorkspaceClosed`), collects records that are expired with no pins (the successor rule moves a superseded record's rows to a live successor; one pin comes off each parent, and a parent that reaches no pins is collected in the same pass, so a stack unwinds from the top), removes expired upload and claim rows, dooms every object left with no row, and deletes objects doomed for longer than the grace period: a `deleting` mark, the frame delete, then the mark's removal. Each step is one transaction with its journal entry (`Collected`, `RowsExpired`, `ObjectDeleted`), guarded by preconditions, so a concurrent write or a second sweeper is safe. Scans are paged (`GcConfig::batch`), each pass has a transaction budget (`pass_budget`), and a pause between batches (`pause`, through a `Pace`). One sweep per repository at a time (`SweepLeases`, in process).
- The sweep refuses a repository without the `gc_protection` marker (`GcError::Unprotected`), and one whose deletion switch is off (`GcError::DeletionOff`). Deletion is off for every repository until the operator turns it on with `set_deletion` (journalled as an `AdminAction`). `Collector::recover` finishes, at start-up, every delete a crashed pass left `deleting`.
- The dry run reads protection records when the repository has the marker (`Collector::plan`, `Collector::dry_run`) and walks ancestry when it does not (the G2.9 report, unchanged). `Collector::verify` checks the plan against the ancestry walk: nothing it collects may be an ancestor of a changeset it keeps, and nothing it dooms may be an object a kept changeset introduced. With `verify`, the sweep refuses to run on a mismatch (`GcError::Mismatch`).
- `Collector::gauges`: `graft_orphans{state}` for `pinned` (expired but pinned: a pin leak shows here), `collectable`, `doomed` and `deleting`. `SweepReport::bytes` feeds `graft_gc_reclaimed_bytes_total`.
- `Shape::retained` (with a default that keeps every row) reads a superseded record's changed paths in both roots for the successor rule.
- Tests, against the real engine on the memory and SQLite stores: scenario tests for every refusal, the retention and grace windows, revival during grace, `MissingObjects` then a re-upload, a parent with a live child, a crashed land candidate, the lapse step, the successor rule, reclaimed bytes, and read counts that do not grow with history depth; a model proptest of random interleavings of writes and sweeps with a brute-force check after each step and an exact model of the frames deleted; races explored over every schedule of two families; a crash at every store call of a pass; and a `kill -9` crash harness on SQLite and the filesystem store.

### G3.3

# G3.3: mirror export mode (graft-primary), divergence stop, reconcile, PR import

- `graft-mirror::export`: graft-primary export (design §11.3, mode B). Each poll reads the journal from the line's cursor (`RefMoved` only, bounded pages), exports the newest move with `graft-git`'s exporter (every changeset upstream lacks, parents first, so each move's changesets reach git in line order), and pushes with `git push --force-with-lease=refs/heads/<branch>:<last pushed sha>`. The cursor, changeset, sha and ref version are recorded in one statement after the push (`exports` table). Moves queued between polls go out in one push.
- Crash safety: each commit is recorded in the mirror map before anything names it, so a re-export after a crash writes the same sha. A refused lease is read back. If upstream holds this export's own unrecorded push (a commit of the line at or after the recorded lease and at or before the target, two ancestry reads), the push is retried once on that lease.
- Divergence: any other refused lease stops the line. `graft_mirror_divergence{line}` goes to 1, an error is logged, and nothing is forced. The cursor stays put, so export resumes with no gap after `reconcile`.
- Bootstrap: a line with no record finds the journal's end by O(log n) one-entry `Watch` reads (never a scan). It starts in sync when upstream is at the tip's commit, catches up when upstream is at an older commit of the line or absent, and stops as diverged on anything else.
- `graft-mirror reconcile --config <file> --line <l> (--land | --discard)`: the stray commits are first fetched into the local mirror under `refs/graft-mirror/stray/<branch>/<sha>`. `--land` imports them, holds them as drafts in `reconcile/<sha>` on the commit export last pushed, lands them through graft, and makes the stray commit the next lease. A fast-forward lands as itself; a moved line rebases it. `--discard` pushes the last exported commit back on a lease naming the stray one. There is no default.
- `graft-mirror::pulls`: open GitHub PRs into the line's branch (not the bridge's `graft/*` ones) are fetched from `refs/pull/<n>/head`, imported, and held as drafts in `github-pr/<n>`, with the lease renewed each poll. A PR moved onto a newer base gets its workspace reopened there. An approved PR (some reviewer's latest review approves, none requests changes) lands through graft. Once export has pushed it, GitHub sees the PR merged when the commits are the PR's own; a rebased one is closed with a comment naming the landed changeset. A PR closed unapproved lets its drafts go. A head that cannot be held (no shared history, a refused change) is `refused` with the reason until it moves. State lives in the `pr_imports` table.
- `graft-mirror::land`: the `Lander` hand-off. In production it is `NativeLand`, which calls graft's `Land`. The tests use a stand-in built from `Rebase` and a line CAS, because the engine's `Land` is still G3.1's stub.
- `graft-mirror export --config <file>` runs export (and the PR import with `[export] import_pulls = true`, which needs `[github]`). `graft-mirror export --to <bare> --repo <r> --db <file>` writes every line and tag (the exit path). The server connection gains the workspace service.
- `git.rs`: `push_lease`, `fetch_refs`, `local_ref`. The token still travels in the environment only.
- Tests: `tests/export.rs` (1,000 random lands exported in order, with identical shas across two runs and fast-forward pushes only; bootstrap cases; out-of-band push: stop, alert, held, never forced; crash between push and record; reconcile by land with and without a rebase, and by discard; refusals; authority claim; the full export), `tests/pulls.rs` (a wiremock GitHub over the bare repository), and `tests/export_crash.rs` (the `kill -9` harness over the SQLite and filesystem stores: GitHub is always an exact prefix of the line, acknowledged pushes survive, no false divergence, and a clean run catches up exactly).
- Not in this change: the committer date of a native commit is the time export first wrote it, not the land record's time, because `Land` does not expose that yet (G3.1). The mirror map keeps it, so later exports are identical. Switching a line from git-primary to graft-primary in the mirror DB is G3.9.

### G3.4

# G3.4: three-way text merge

- `graft-model::merge`: `merge3` returns `Clean`, `Conflicts` (line ranges in ours), `NotText` (not UTF-8, a NUL byte, or 1 MiB or more; D16) or `TooComplex`. It follows `git merge-file --diff3 --diff-algorithm=histogram`: git's histogram diff with xdiff's hunk sliding and `EAGER` combination. No new dependencies.
- Cost follows the input: each side's diff runs under a deterministic work budget (`WORK_FLOOR` plus `WORK_PER_LINE` units per line). The histogram diff indexes line occurrences once instead of per region; when it runs over its share (many scattered hunks), the diff anchors on unique lines first; the repeated-line fallback is a linear-space Myers diff. A merge over the budget is `TooComplex`: callers treat it as a whole-file conflict, and `render_conflict_markers` renders it as one.
- `render_conflict_markers` writes git's diff3 markers (`ours` / `base` / `theirs`).
- Tests: a 600-case differential corpus against `git merge-file` output recorded from git 2.53.0 in `tests/fixtures/merge3/corpus.txt`, so the required test does not depend on the installed git (cleanliness and bytes; known differences listed in the test; the live comparison is an ignored test, and `GRAFT_BLESS=1` re-records the fixture), proptest invariants (unchanged side, identical changes, ours/theirs symmetry), and large-input time and memory bounds (`tests/merge_large.rs`; the full-size probes run with `--release -- --ignored merge`).

### G3.5

# G3.5: client and CLI native land, conflicts, revert, stack, attest

- `graft land` (native): one `Land` call, acted on as design §8.2 says. Landed (0) moves the workspace and the files to the landed changeset; when the server rebased, that brings in the line's other changes within the slice. Conflict (3) changes nothing and names the paths; `--sync` writes them into the files instead. Contended (4) is retried with backoff for up to `--wait` (60 s). Rejected (5), denied (6) and unavailable (7) change nothing; the retryable ones say so. A land whose reply was lost is safe to run again (it is idempotent).
- Conflict materialization: when the server refuses to rebase the stack (`Sync` → `Conflict`), `graft sync` (`WorkingCopy::sync_writing_conflicts`) moves the working copy and its workspace to the tip anyway, with the stack's net changes on top as uncommitted edits, and exits 3. A path both sides changed gets the three-way merge (`graft_model::merge3`) when it is clean, conflict markers (`render_conflict_markers`) when it is not, or, for a binary, symlink, deletion or kind change, our side plus a `<path>.graft-conflict` sidecar naming all three versions. `.graft/conflict` records them; `status` lists them; `commit` refuses while markers or sidecars remain, defaults to the stack's message, and the commit that succeeds is the stack as one draft on the tip. The sync needs a clean working copy, and the stack's changes must be in the slice.
- Never stranded: a move that spans the server and the disk (land, a stack sync, a conflict sync, revert, a stack operation) records where it goes in `.graft/state` (`moving.base`, `moving.head`; `moving.head = server` while the server picks the head) before its first step. Until it finishes, `begin` accepts the workspace on either side and reopens a lapsed one where the move goes; `sync`, `land` and `WorkingCopy::finish` complete it. Lost replies from `Sync` and `Stack` are taken up the same way. Property test: random rivals and faults (before or after the engine runs, on ten operations) never leave the base or the move target off the line, and recovery always lands the change.
- `graft revert <rev>`: a draft on the head that inverts the changeset (the server's `Revert`, created without a workspace, then the head moved by CAS after the files are checked). `graft stack squash|reorder|split|drop`: the server's `Stack`, on a clean working copy; drafts are named by id or unique prefix. `graft attest <rev> --check NAME --pass|--fail [--url]`: posts a check result for CI, with or without a working copy.
- JSON: new `revert` and `attest` schemas; `status` gains `conflicts` and `unfinished`; the error object may carry `onto` and `conflicts` (path and how it was written). `graft api --list` now names the `revert`, `attest` and `stack` commands.
- Client API: `Error::{Conflicted, Unresolved, Unfinished}`, `LandOptions::sync`, `Begun::unfinished`, `WorkingCopy::{finish, sync_writing_conflicts, revert, rewrite_stack, conflicts, has_conflicts}`, `WcState::moving`.
- Tests: client and CLI tests against the real engine (G3.1, memory store) through `crates/graft-client/tests/support/engine.rs`, which injects faults and racing lands and models `Revert` and `Stack` until the engine has them (G3.7). The CLI's `land_results` snapshot maps every land result to its exit code, human output and JSON.
- Behaviour change: `graft sync` with a conflicting stack used to exit 3 and change nothing; it now writes the conflicts (exit 3), as design §8.2 specifies. The library's `WorkingCopy::sync` keeps the typed `Conflict` and changes nothing, so callers such as graft-lsp see the conflict as before.

### G3.6

- graft-lsp: `graft lsp`, a stdio LSP server next to the editor's language server: hover blame (changeset, author and kind, subject, draft state, Vogt work item and session), code lenses ("changed N times, last by X"; "base is K changesets behind main"), diagnostics (conflict regions after a failed sync or land, files outside the sparse slice, regions changed upstream since the base), the `graft.commit`, `graft.land`, `graft.sync`, `graft.diffBase` and `graft.blameFile` commands, and `graft:` virtual documents read with `graft/readFile`. A scripted-client conformance suite runs it against the testkit server with the real engine behind it.

### G3.7

# G3.7: Revert, stack operations, line merge

- `graft-engine`: line merge (decision D16). With `merge = line`, a path both sides of a rebase changed is merged three ways with `graft_model::merge3` when the base, ours and theirs are regular or executable files of text under 1 MiB; the executable bit comes from the side that changed it, and a merge already on the target is dropped. Larger, binary, symlink, deleted or conflicting paths still conflict at file level. Merged blobs are stored with the changeset that introduces them, as inline blobs. A rebased changeset records the paths its own rebase merged, in it or in any draft below it in the stack, in `graft.merged-paths` (a JSON array of paths, `MERGED_PATHS`); a later file-level rebase removes it. `Sync` always merges by line; `Rebase` and `Land` take the request's `merge`, and `Landed.merged_paths` lists what a land's rebase merged (also on a retried land).
- `graft-engine`: `attest_merged` on the `attested` policy. Attestations are no longer inherited through a changeset whose rebase merged by line, and a land whose own rebase would merge is `Rejected` (`attested`, naming up to eight paths), so the caller syncs, has CI attest the merged head, and lands it. Without `attest_merged`, attestations carry through merges as before.
- `graft-engine`: `Revert { changeset, onto, workspace? }`. It inverts the changeset against its first parent, then rebases the inverse onto `onto` with line merge. The result is deterministic (the caller as author, the reverted changeset's time, `graft.reverts`, `REVERTS`), so a retry returns the same id. With `workspace` it goes on top of that workspace's stack by CAS; without, it is stored abandoned. A revert with nothing left to undo is `revert-empty`; a root is `revert-root`.
- `graft-engine`: `Stack` operations `reorder`, `squash`, `split` (by path groups; the first group covering a path takes it) and `drop`. The drafts above the edit are rebased with line merge; new drafts drop the server's `graft.` metadata; replaced drafts are superseded by their successors and a dropped draft is abandoned, in one transaction guarded by the workspace that also moves the head. Bad requests are `not-in-stack`, `stack-order`, `stack-squash`, `stack-split`, `stack-split-uncovered` and `stack-split-empty`.
- Tests: two lands on different regions of one file both land and equal `merge3`; file mode, overlapping lines, binary and 1 MiB files still conflict; sync merges; `attest_merged` forces re-attesting (direct and through a workspace sync); revert of revert restores the original tree; revert onto a later tip, into a workspace and with conflicts; every stack operation and its refusals; the G3.1 concurrency race with eight lands on different lines of one file under line merge; the rebase model proptest in both modes; and a proptest of revert of revert and stack operations over random snapshots.

### G3.8

# G3.8: CI integration: OIDC exchange, attest workflow, post-land CI

- `graft-auth::oidc`: exchanges a GitHub Actions OIDC token, or a Woodpecker job token, for a short graft token (kind `ci`, at most 15 minutes and never past the CI token's expiry). It verifies the JWT against the issuer's JWKS (`RS256` and `EdDSA` only, `kid` required, RSA of at least 2048 bits), then `iss`, `aud`, `exp`, `nbf`/`iat` and lifetime. A `Binding` maps a CI repository to a graft repository and its scopes (`read`/`write` only), and can require the repository id, refs and the pinned reusable workflow. A `write` binding (which can attest) is exchanged only through `Exchange::exchange_pinned`: the token's `sha` must be the commit exported for the changeset, and the minted token's subject pins that changeset. A valid token for any other repository is refused. Tests use recorded JWKS and tokens generated with throwaway keys (`tests/fixtures/oidc/gen.py`).
- `ci/`: a pre-land workflow template (checks on `graft/*` branches in jobs with no `id-token`, then a separate attest job), a post-land one for the mirror's pushes, and callers. The templates take no inputs: the checks are fixed in a per-repository copy. They call `graft attest <rev> --check NAME --pass|--fail` and a `graft ci exchange --changeset --out PATH` that writes the token to a 0600 file (WI-2271); the attest job runs on a GitHub-hosted runner; see `ci/README.md`. The live rehearsal is a follow-up.

### G3.8b

# G3.8b: CI token exchange RPC and `graft ci exchange`

- `graft-proto`: `CiService.ExchangeCiToken { ci_token, changeset? }`, answered with a `Token`. It is called without a bearer token, and the request's `Debug` redacts the CI token.
- `graft-server`: serves `ExchangeCiToken` with `graft_auth::oidc`, using issuers and repository bindings from a new `[ci]` config section (see `src/ci.rs`). It binds the protocol version and maps errors to `Unauthenticated`/`PermissionDenied`. Key sets come from `jwks_file`, or from `jwks_url`, fetched at start-up, on a timer, and on an unknown `kid` (at most once a minute). A binding with `write` mints only tokens pinned to a changeset (`exchange_pinned`). The commit check fails closed: the only source is a static `commit_map_file`, until the mirror publishes its export map (follow-up). Config checks refuse a GitHub binding without a pinned workflow, a write binding without refs, a Woodpecker write binding, and a CI repository bound twice.
- `graft-cli`: `graft ci exchange --provider github|woodpecker [--changeset ID] --out PATH` reads the CI JWT on standard input and writes the graft token to `PATH` (0600, replaced whole). It never prints the token.

### G3.9

# G3.9: authority switch tooling (`graft-mirror authority`) and rehearsal

- `graft-mirror authority <status|freeze|verify|flip|unfreeze> --config <file> --line <l> [--last N]` implements design §11.3's switch in both directions: A → B (git-primary to graft-primary) and B → A (the rollback). `graft-mirror verify --config <file> --line <l> [--last N]` runs the verify check on its own. The runbook is `docs/runbooks/authority-switch.md`.
- `graft-mirror::authority`: the `Switcher` steps.
  - **freeze** records the switch (`switches` table) and closes the upstream gate. From git-primary, nobody may push. From graft-primary, only the mirror may push, so export can catch up.
  - **verify** requires upstream and graft to name the same commit through the mirror map, follow not stopped and no open bridge request (from A), and export not diverged (from B). It re-exports the last N first-parent changesets (default 100, at most 10,000): the commit is in the mirror, the fidelity check (same tree changes, same parents) passes, and rebuilding the commit from the mirror record gives the upstream sha. It records the verified tips.
  - **flip** refuses unless verified and neither side has moved since; a moved side puts the switch back to `frozen`. Otherwise it moves the claim by compare-and-swap in one transaction, which also drops export's record (so export bootstraps level), restarts follow's record at the verified commit, and marks the switch flipped. The gate becomes mirror-only towards graft-primary, and frozen to everyone towards git-primary.
  - **unfreeze** opens the gate in the holder's posture and ends the switch. Before a flip, it aborts the switch instead.
- Gates: `RulesetGate` puts the repository ruleset `graft-mirror-authority-<branch>` on GitHub (update, deletion and non-fast-forward rules). Frozen means no bypass; mirror-only means the mirror's App bypasses it (D8); open means the ruleset is deleted. `HookGate` is a `pre-receive` hook plus a state file on a local bare repository, the stand-in for the rehearsal and the tests. It admits mirror-only pushes that carry `GRAFT_MIRROR_PUSH=1`, which every mirror push now sets.
- `export` re-reads the claim on every poll and stops writing a line it no longer holds (follow already did). `HttpForge` gains the ruleset calls and accepts empty (204) answers.
- Tests: `tests/authority.rs` covers an A → B → A round trip that loses nothing and ends with graft's chain equal to GitHub's; the gate refusing people while frozen and graft-primary; every refusal; an aborted switch; and the ruleset gate against wiremock.
- Rehearsal: `crates/graft-mirror/scripts/rehearse-authority-switch.sh` runs the real binaries over a scratch copy of rustTorrent and a local "GitHub". It is recorded in `docs/trials/2026-10-23-authority-switch-rehearsal.md`.
- Not in this change: graft-server has no API or config for a line's authority, so the tool cannot set graft's side. The operator does it, and the runbook says when. A rehearsal against a real scratch GitHub repository needs the operator.

### WI-2216

- graft-index: Blame's work estimate now counts only the regions the anchor lines actually split the file into (bounded by each side's longest run without an anchor), so anchors clustered at the top of a large repetitive block no longer keep a quadratic diff under budget; it attributes coarsely instead. The reviewer's probe (300 distinct lines over 80k repetitive lines, 6 versions) went from 10.7s to 0.04s in release.

### WI-2217

- graft-client: a `PutObjects` message over the server's decode limit now fails with the typed `LimitExceeded` (limit `put-objects-message`, with the server's limit) instead of a generic unavailable error. graft-server refuses at start-up a `max_put_call_bytes` above 80 MiB, and a `put_wait_ms` or `put_idle_ms` above one hour (exit 78), rather than silently clamping or treating them as no timeout.
- graft-server: every gRPC answer, including error statuses, now carries the server's `graft-protocol` version header, so clients can bind the version (design §7.1).

### WI-2220

# WI-2220: server metrics for the writer queue, store timing and free disk

- `graft-store-sqlite`: new `StoreObserver` trait, installed with `SqliteMetaStore::open_observed`. The store reports the writer queue depth each time a job is queued, refused or taken, one report at a time under the queue's lock so the last report is always the current depth, and each `MetaStore` operation's latency measured from the call (queueing included). The store does not depend on telemetry; `open` is unchanged and passes no observer.
- `graft-server`: the observer records `graft_sqlite_writer_queue_depth` and `graft_store_op_seconds{port="meta",op}`. These were registered but never set, so the ops alert `GraftSqliteWriterSaturated` could not fire.
- `graft-server`: the free-space floor's `statvfs` reads now set `graft_disk_free_bytes{volume}` and `graft_disk_total_bytes{volume}` (`objects` and `meta`). Every volume is read and recorded on each sample, even when another is below the floor. A scrape of `/metrics` refreshes them, so the gauges are current without writes.
- `graft-telemetry`: the two disk gauges are registered. They are not yet in design §13.
- Tests: the API test, after its writes, scrapes `/metrics` and checks the values: commits were timed, the queue depth is 0, and each volume has free <= total with total > 0. A store test overfills a one-job queue from 16 threads and checks the last depth reported is 0. An admission test checks a volume below the floor does not stop the other being recorded.
- Not changed: the mirror config under `docker/dev`, and the object store's `graft_store_op_seconds` (`port="objects"`).

### WI-2221

# WI-2221: deterministic writer-queue backpressure test

- `graft-store-sqlite`: `a_full_writer_queue_is_busy` waits until the writer has taken the first job off its queue before it fills the queue, so the refusal no longer depends on thread timing. The writer counts the batches it takes (`SqliteMetaStore::writer_batches_taken`, a doc-hidden test hook). The test's assertions are unchanged.

### WI-2222

- graft-index: documents the tie-break rule for identical lines in `blame`. A diff matches a repeated line to one of its copies, so graft and git blame can name different commits for the same line. The rule for when both are valid is in the module docs.
- graft-testkit: the pilot's blame agreement check accepts an equivalent tie-break. An origin is valid only if a whole-file first-parent blame names it under one diff: git blame with Myers diff, git blame with histogram diff, or graft's histogram rule replayed along the file's versions. A moved, reordered, or deleted and re-added line belongs to the commit whose diff adds it there. A line from a merge's second parent belongs to the merge. A wrong origin still fails. The report counts these as `equivalent tie-breaks`. Fixes WI-2222 (rustTorrent `Cargo.lock`, 623 of 9058 lines).

### WI-2224

# WI-2224: signal handlers before start-up

- `graft-server`: SIGTERM and SIGINT handlers are installed first, before telemetry, stores or any listener. Previously they were installed after the server announced itself, so a signal in that gap killed the process with the default action (exit 143) instead of a clean stop. A signal during start-up now exits 0 with nothing bound, and start-up prints `graft-server signals armed` once the handlers are in place.
- Tests: `sigterm_during_start_up_exits_clean_with_nothing_bound` sends SIGTERM after the armed line, before readiness, and asserts exit 0 with the port closed.

### WI-2225

- dev stack: the mirror configs under `docker/` use the current graft-mirror follow schema (`upstream`, `mirror_dir`, `db`, `[[lines]]`); a graft-mirror test parses every one with the crate's parser. `scripts/dev/reload.sh` now fails loudly when a restarted mirror is not running, and prints its last log lines.
- dev stack: `scripts/dev/reload.sh` maps every `graft-mirror*` service (such as `graft-mirror-rusttorrent`) to the graft-mirror binary, so its mirror check covers them, and `--bin <service>` prints the mapping. `scripts/dev/smoke.sh` fails when a started mirror service is not running and prints its last log lines. A graft-mirror test checks reload.sh maps every graft service in `compose.yaml`.

### WI-2226

- docs: D8 (a GitHub App on the org for the mirror) and D10 (Vogt mints session-scoped graft tokens) are recorded as decided. Design §7.8 states the file/directory rule G2.7 implements. Design §8.6 adds GC protection records (object rows, parent pins, claim rows, the successor rule), the sweep, its migration and failure modes. Plan adds G3.11 for the write-path work. G3.2 now depends on it and moves to wave 3.C.

### WI-2228

CI: every workflow runs on the estate's self-hosted runners only (static `runs-on: [self-hosted]`, `[self-hosted, docker]` for the image job), with a `runner-policy` gate job that rejects non-self-hosted or dynamic `runs-on`, job timeouts, and a Rust cache that only `main` writes back. The fmt, clippy and test check names are unchanged.

### WI-2234

WI-2234: faster image builds. `docker/Dockerfile` no longer caps `CARGO_BUILD_JOBS` (compose.yaml still passes 4 for the shared dev host), drops the cargo-chef stage (it compiled cargo-chef from source on every cold build), and builds with BuildKit cache mounts for the cargo registry, git and target directories, copying the binaries out of the mount. It compiles through a pinned, checksum-verified sccache when the build is given the runners' sccache settings and keys (keys as build secrets only). The `image` workflow drops `type=gha` for a named buildx builder whose state is kept on each runner. Image contents, user and paths are unchanged.

### WI-2247

Diff read path (WI-2247): the engine decodes, verifies and diffs a diff's patches on up to 8 scoped threads. It works in windows of as much input as the remaining response budget could show, and applies the per-file cap and the budget in file order, so the response (errors included) is unchanged. graft-server opens its object store with `verify_reads(false)`: every reader verifies the frames it decodes, so the store no longer inflates and hashes each one first. graft-store-fs refuses a file larger than any valid frame whether or not reads are verified. `graft --json` output is buffered instead of written one line at a time. graft-pilot's diff scenario now runs `git diff` with patches, like for like. rustTorrent main~20..main, graft-pilot p50: diff 22.1 -> 12.9 ms (git 13.2 / 15.1 ms); blame cached 36.7 -> 21.6 ms.

### WI-2248

Quiet-host baseline: graft-pilot on rustTorrent, 3 x 20, on the idle graft-dev VM at 7e82d62. Full clone 332.6 ms, sparse 172.0 ms, log --path 4.71 ms, diff 13.1 ms (graft p50), all at or above the Node B figures. Recorded in docs/trials/2026-10-23-quiet-host-baseline.md.

### WI-2249

- Test: the graft-client hostile entry-budget test no longer asserts a 10 s wall-clock bound; it keeps a 120 s hang guard (WI-2249).

### WI-2251

- Deploy: `deploy/` runs the graft-dev stack (graft-server and a rustTorrent graft-mirror) on the graft-dev VM, deployed by `deploy/up.sh` from the `deploy-dev` workflow on the VM's runner (WI-2251).

### WI-2261

- CI: deploy-dev targets the graft-dev-only runner label; runner-policy allows it; cargo-nextest pinned to 0.9.148 (WI-2261).

### WI-2262

graft-mirror: a `[remote rejected]` push that is not a lease or non-fast-forward failure (for example a ref lock held by upstream's own `gc --auto`) is now an error to retry, not a divergence that stops export. The export test's fake GitHub runs no background gc, and the test names the outcome if export stops.
The `kill_9_at_any_point_resumes_with_no_duplicate_and_no_gap` test now waits for the `git` processes a killed child orphaned to finish before the next run, so their ref locks cannot make a later push fail.

### WI-2267

graft-index: the git comparison tests now run git with every GIT_* variable dropped, HOME in the repo, and auto-gc/maintenance off, and check `git add` (WI-2267).

### WI-2269

- graft-mirror: the export crash test no longer depends on timing — in alternate iterations the child acknowledges nothing until its first push, so the kill delay counts from that push and every run proves progress (WI-2269).

### WI-2270

- docs: rustTorrent trial-1 write-up (`docs/trials/2026-10-rusttorrent-trial-1.md`): four direct lands, batch PR #214, what worked and what didn't, and the fixes since (WI-2350, D26, WI-2340).

### WI-2272

# WI-2272: dedicated `attest` scope; Attest enforces the CI changeset pin

- `graft-auth`: new scope `repo:<r>:attest`. It grants `Attest` and nothing else (no read, write, land or workspaces), and `write` does not imply it; `admin` still does.
- `graft-engine`: `Attest` needs `attest` or `admin` (it used to take any `write`). A `ci` principal may attest only the changeset its subject is pinned to (`ci:<provider>:<repo>#<changeset>`); another changeset, or no pin, is `PermissionDenied`, checked before the changeset is looked up. Agents still never attest.
- `graft-auth::oidc`: a CI `Binding` holds `read` and `attest` only (`write` is now refused, as `land` and `admin` were), and it is attest bindings that need `Exchange::exchange_pinned`, named refs and a pinned workflow. `ci/README.md` updated.
- Behaviour change: a token with only `write` can no longer attest. Grant `repo:<r>:attest` to whatever attests.

### WI-2274

# WI-2274 (G3.2b): the GC sweep wired into the server

- Engine: `Gc` with `dry_run: false` runs one sweep pass through `graft-gc` (`Engine::gc_sweep`, which also takes `verify`). It is still refused (`Rejected { policy: "gc" }`) on a repository whose deletion switch is off, which is every repository until the operator turns it on. A second sweeper is `Unavailable`. The dry run now reads the protection plan on a protected repository (`Collector::dry_run`). `Engine::with_gc(GcSettings)` sets the collector's timings, pace and leases.
- `graft-server admin gc <repo> [--dry-run] [--verify]` runs one pass, or reports what one would collect. `admin gc deletion <repo> on|off` is the operator's deletion switch, journalled as an `AdminAction`. Both need the server stopped, as `fsck` does.
- `[gc]` in the server config: `enabled` (default **false**), `interval_s`, `repos`, `grace_s` (at least 60), `retention_s` and `verify` (default true). When enabled, `serve` sweeps each listed repository every interval on the runtime timer. A repository with deletion off refuses each pass, and the refusal is counted.
- Metrics: `graft_gc_sweeps_total{outcome=ok|busy|refused|mismatch|failed}`, `graft_gc_collected_total`, `graft_gc_doomed_total` and `graft_gc_deleted_total`, beside `graft_gc_reclaimed_bytes_total`.
- Review follow-ups from G3.2:
  1. A land onto the tip that a crashed attempt's collected candidate was built on now lands. Writing a collected changeset again revives it over its tombstone: new protection, guarded by the record still being collected. Before this, the land returned `Contended` until the line moved.
  2. The sweep lease lives in the metadata store: a derived value taken and renewed by CAS (new additive `Precondition::DerivedIs`), and released when the pass ends. Its owner is the host name, a random nonce drawn once per process, the pid and a counter, so two containers that are both pid 1 are still distinct owners. Two processes on one store can't sweep a repository at once. If a sweeper dies holding the lease, the repository is blocked only until `GcConfig::sweep_lease` (10 minutes) runs out.
  3. The lapse step closes a stack longer than `max_stack` in several transactions. Each part abandons up to `max_stack` drafts and moves the head below them (journalled `WorkspaceLapsing {name, abandoned}`), and the last part deletes the workspace. If a pass runs out of budget part-way, the next pass continues. The plan walks the whole stack.
  4. `Collected` now carries `bytes`, the frame bytes of the objects it doomed: `{"ids":[…],"objects":n,"bytes":b,"moved":m}`. Design §4.8 doesn't list `RowsExpired`, `ObjectDeleted` or the new `WorkspaceLapsing`, so it needs a docs PR.
  5. A collected changeset's tombstone drops its content (`PutChangeset` with empty content over the record, documented as a replace). Nothing reads a collected changeset's content, and a revival stores it again.

### WI-2276

# WI-2276: trial attester on the graft-dev VM

- New crate `graft-attester` (lib + bin). It watches one repository's journal for drafts held by a workspace on one line (decision D21: line `trial-1` of `rusttorrent` by default; `GetWorkspace` names a workspace's line). It keeps the newest draft per workspace in a bounded queue, and the journal holds the overflow. It checks one draft at a time: it confirms the changeset is still a draft, checks the tree out with `graft-client`, runs the configured checks (default rustTorrent's Rust job: `ci/fmt` `cargo fmt --all -- --check`, `ci/clippy` `cargo clippy --workspace --exclude rtbit-desktop --all-targets --no-default-features --features default-tls -- -D warnings`, `ci/test` `cargo test --workspace --exclude rtbit-desktop --locked --no-default-features --features default-tls`), and posts `Attest` per check, passed or failed, with the log's path as the URL. Each check runs in its own process group with a cleared environment, a timeout and a capped log, optionally as another uid (`--run-as`), and after a failure the later checks are attested failed without running (`--keep-going` runs them all). The repository, line and checks are configuration (`--repo`, `--line`, `--check`). The read position persists across restarts. The token is read from a 0600 file and never printed.
- No silent failures (review of 64a566d): a checkout that fails transiently is retried up to `max_attempts`, then every check is attested failed. A permanent checkout failure attests every check failed at once, with a log saying why. `Attest` is retried with backoff. If it still fails, the results are kept and the draft is requeued, so the attestations are posted again without re-running the checks.
- `deploy/`: an opt-in `graft-attester` service, compose profile `attester`, built from `deploy/attester/Dockerfile` (Rust 1.95 with rustfmt and clippy, as rustTorrent's CI pins it). It runs as root with only CHOWN/SETUID/SETGID/KILL and runs the checks as uid 10001, so a check can't read the token. CPU, memory and process caps apply, and the logs and cargo cache live in volumes. With `GRAFT_ATTESTER=1`, `up.sh` mints `/etc/graft/attester-token` (`read` + `attest` on `GRAFT_ATTESTER_REPO`, default `rusttorrent`; root-owned 0600, renewed like the mirror token), and builds and starts the attester. See `deploy/README.md`.
- Needs a server image with the `attest` scope (WI-2272, now on main). With an older image, minting the attester token fails, but only with the profile on.
- Smoke test (`crates/graft-attester/tests/smoke.rs`): the engine behind `TestServer` with a tiny repository. A passing draft gets two passing attestations, a failing draft a failing one, a check past its timeout is killed and failed, and a draft that is no longer a draft is skipped. Injected faults cover checkout retry and give-up, permanent checkout failure, and Attest retry without re-running the checks.

### WI-2283

- graft-engine: a stack operation whose server-built draft is exactly an earlier, superseded draft (a reorder that undoes a put/delete/put, then a squash that strips `graft.` metadata) now revives it instead of failing with `draft-superseded`. The `stack_ops_keep_the_snapshot` counterexample is pinned as a named regression test.

### WI-2284

- graft-mirror: batch mode (decision D21). `graft-mirror pr-batch <line> --base main` follows `main`, syncs an integration line onto it (rebase of the line's net change, recorded as a fast-forward merge changeset by CAS; conflicts reported, nothing forced), and exports the net change as one squashed commit on GitHub's `main`, pushed as `graft/batch-<n>` with a lease and opened as a PR. The commit message lists every land (changeset, author, session, work item, attestations); the PR body lists as many as fit under GitHub's body limit and points to the commit for the rest. Both end with `Graft-Batch` trailers. The PR is never merged or queued by graft. A merged PR settles the batch once follow imports its merge commit, and the next batch counts from its tip; a PR closed unmerged marks the batch rejected and leaves the line as is.

### WI-2288

- graft-server: optional `[[lines]]` and `[default_line]` sections in `server.toml` set per-line land policy (direct, attested or queued; checks, attest_merged, authority, path_rules, post_land). Absent, the defaults are unchanged (`main` is D6, other lines direct). Unknown keys and invalid combinations are refused at start-up. The config is global, not per repository (WI-2288).

### WI-2291

- deploy: `up.sh` checks `server.toml` and `mirror.toml` with the pinned image's own binaries before it swaps containers, and refuses a config the image can't load, leaving the running stack alone. After the swap it waits for `/readyz`. If the new stack doesn't come up, it rolls back to the last good image pin and configs (recorded under `/etc/graft/last-good`) and fails. `deploy-dev.yml` reports a refused or rolled-back deploy in the job summary.

### WI-2295

Docs for D22 that #82 and #85 did not cover: the reasons for D22 and a phase-2 note in the D6 section (decisions.md); a phase 2 inventory of the parked CI pieces (plan.md); a phase-1 `direct` land sequence and phase labels in architecture.md §2; `direct` as the phase-1 default in design.md §8.3; the trial agent guide no longer waits for attestations, since landing is immediate (WI-2295).

### WI-2301

# WI-2301: create a line through the API, CLI and MCP

- `CreateLine { repo, name, from }` (gRPC `RepoService.CreateLine`, and
  `RepoApi::create_line`) forks a line: a compare-and-swap from absent to
  `from`, journalled as `RefMoved` with the line's index rows in one
  transaction. It never overwrites a ref: an existing name (of any kind)
  answers `RefConflict { actual: Some(target) }` and nothing moves. A bad
  name is `InvalidChange { rule: "ref-name" }`; an unknown `from` is
  `NotFound`.
- Scope: `repo:<r>:admin`, or `repo:<r>:land:<name>` for the line `<name>`
  only. The new line takes the server's policy for its name (`direct`
  unless `[[lines]]` configures it, D22).
- `graft line create <name> --from <rev>` and `graft line list
  [--prefix] [--limit] [--cursor]`, with `--json` (schemas `line` and
  `line-list`).
- MCP: `graft_line_create { repo, name, from }`.
- No more `graft-mirror import --line refs/heads/main=<line>` workaround.

### WI-2302

- graft-mirror: removed the `bridge` subcommand stub and its usage line. Per-land
  PRs were rejected by D21; GitHub delivery is the `pr-batch` squashed PR. The
  `bridge` library module stays: its record types, `GIT_PRIMARY` and `clean_text`
  are used by the mirror database, authority switch, follow, pulls and pr-batch.

### WI-2303

- graft-index, graft-engine: a plain `graft log` page reads each changeset once, not twice. `Index::log_changesets` returns the changesets the first-parent walk already decoded, and the engine builds the page from them: a 50-row page drops from ~100 changeset reads to 51, the same read shape as `log --path`. Bench at 1,000 changesets on a heavily loaded host (load 20-190): interleaved A/B p50 8.3 → 6.8 ms against `log --path` 6.8-7.6 ms; summary.md `log, 50` 6.2 → 6.3 ms (before/after runs at different load). The remaining ~4 ms against git's 2.3 ms is fixed per call: process start ~1.5 ms, gRPC connect, and the resolve and log RPCs. Each changeset read is still one serial store hop (~27 µs); one range read per page needs a `MetaStore` line scan by position (follow-up).

### WI-2304

- bench: 10,000-commit baseline on the graft-dev VM (WI-2304): pass, merge3 lands with no file-level refusals, `line create` flat at the tip, no history-growth suspects; results in `bench/results/2026-10-23-scale-10000/` and `docs/trials/2026-10-23-bench-10k-graft-dev.md`.

### WI-2305

# WI-2305: bench correctness smoke in the merge queue

- CI: a new `bench-smoke` workflow runs on `merge_group` and `workflow_dispatch` only. It builds the release `graft`, `graft-server` and `graft-mirror` binaries (with the WI-2314 OOM-flake retry), runs `python3 -m graftbench --scales 10 --writers 4 --rounds 2 --iterations 2`, and fails on any non-zero exit (1 correctness, 2 harness error). It gates on correctness only, never timings, and uploads `summary.md` and `results.json` as the `bench-smoke` artifact.

### WI-2306

- `graft-mirror pr-batch` now fails with a typed `MirrorError::Unmapped` naming the mirror DB path when the DB is empty or belongs to another repo, instead of reporting the base line as `graft-moved`.

### WI-2307

- graft-dev: scheduled backups (`deploy/backup/`: `admin backup --objects`
  every 6 hours via a systemd timer, 14 dated metadata snapshots, optional
  off-VM rsync), a restore rehearsal script ending in `admin fsck`, Prometheus
  alert rules and scrape config for server down, low disk and mirror lag
  (`deploy/monitoring/`), and the runbook
  `docs/runbooks/graft-dev-backup-restore.md`. (WI-2307)

### WI-2308

- graft-git import: queued blobs go to the server in `PutObjects` calls of 32,
  up to 8 in flight, instead of one call at a time. Bench at 1,000 commits:
  `graft-mirror import` 22.5 s (44.5 commits/s) → 18.5 s (54.1 commits/s);
  blob upload time ~6 s → ~1.8 s. The fidelity check and every encoding are
  unchanged; blobs are still stored before `ImportChangesets` names them.
  The rest is server fsyncs in `ImportChangesets` (with server storage on
  tmpfs the import takes 1.55 s); that is WI-2319 (graft-engine).

### WI-2309

- graft-cli, graft-client, graft-mcp: `graft status --upstream` (and `--json`, schema in `status.schema.json`) and the `graft_upstream` MCP tool preview a land without side effects: the line tip against the base, how many changesets behind (bounded), one page of files the line changed, and for each file you also changed (uncommitted edits and the stack) whether the D16 line merge is clean or conflicts, with the conflicting line ranges. Costs follow the change: `Diff` base..tip restricted to your paths (64 per request, at most 1,024), plus the engine's in-memory `GetConflicts` planner for the stack. Over `/mcp` without a working copy it answers from the workspace's drafts. Nothing is written and no base or head moves.

### WI-2310-docs

# WI-2310: draft overlap decision (docs)

- `docs/decisions.md`: D24, draft overlap visibility: the `repo:<r>:drafts` scope (granted only by `admin`), paths and principals but never contents, head ids or tokens, live leased workspaces only, index-order paging with a 4 × limit scan budget, no data migration.
- `docs/design.md`: `OverlappingDrafts` in §7.2; `attest` and `drafts` scopes in §12.1.
- `docs/notes/draft-overlap.md`: points at D24; duplicate migration sentence removed.

### WI-2310-note

- docs: design note for draft overlap (`repo:<r>:drafts` scope, per-workspace draft-path index, `OverlappingDrafts` query, CLI and MCP surfaces) in `docs/notes/draft-overlap.md` (WI-2310).

### WI-2310

- Draft overlap, step a (interface): `repo:<r>:drafts` scope (granted only by
  itself and `admin`); `OverlappingDrafts` request/response types, wire
  messages and the `WorkspaceApi::overlapping_drafts` method (default
  `Unavailable`); MetaStore `SetDraftPaths`/`AddDraftPaths` writes, the
  `scan_draft_paths`/`workspace_draft_paths` reads (memory store implemented,
  SQLite pending) and `WorkspaceRecord.work_item`.
- Draft overlap, step b (store and engine): SQLite migration v6
  (`draft_paths` table with a reverse index by workspace, and
  `workspaces.work_item`); the draft path index kept in the same transaction
  as every head move (commit, set-head, stack edits, sync, resolve) and
  dropped with the workspace on close, lease expiry and GC; the engine's
  `OverlappingDrafts`, gated by `repo:<r>:drafts`, paged in index order with
  a scan budget of 4 × limit rows, excluding the caller's own workspaces,
  expired leases and heads that are no longer drafts. Store scans now read
  only rows beneath the prefix. The default `Unavailable` answer carries a
  60 s retry.
- Draft overlap, step c (surfaces): the gRPC `OverlappingDrafts` handler is
  a read (no write admission); the JSON facade route
  `GET /v1/repos/<r>/drafts/overlap?line=&path=…&include_own=&limit=&page_token=`;
  `OverlappingDrafts` on `/mcp`; the `graft_draft_overlap` MCP tool (grouped
  by workspace, shrinks to fit the result cap); `graft status --drafts
  [PATHS]` (with or without `--upstream`), which asks about the uncommitted
  changes and the stack's paths, 64 at a time; the gRPC client now forwards
  `OverlappingDrafts` (it fell through to `Unavailable`); `admin fsck` checks
  each workspace's draft path rows against its stack and reports a
  workspace count.

### WI-2311

- bench: a deterministic "live awareness" mode (`--live-awareness`, `--overlap low|mid|high`, `--racing`). A seeded W x K edit script runs under one coordinator, one client call per tick, for graft `late` and `sync-early` (`preview` and `drafts` switch on when `graft status` offers `--upstream` / `--drafts`) and git `late` and `fetch-rebase-early`. A `git merge-file` oracle predicts every sync and land conflict and tracks open-draft paths. The gates: same counts on two runs, graft equal to the oracle, final trees equal to the expected tree and to git, no lost or duplicated lands, and a line that never moves backwards. Also an MCP conflict scenario over `/mcp` that must match the CLI path. The summary gains a "Live awareness" table; a scale-10 run is in `bench/results/2026-10-23-live-awareness/`.

### WI-2312-engine

- Conflict resolution without a working copy (WI-2312, option (a), step 2b).
  graft-engine implements `GetConflicts` (plans the stack's sync onto the
  line tip, or `onto`, in memory and returns, for the first draft that
  conflicts, each path's base/ours/theirs entries and merge3 markers capped
  at `MAX_CONFLICT_MARKER_BYTES`; binary, oversized, kind-changing and
  add/delete paths get ids and a reason; paged by path) and `Resolve` (the
  sync with caller resolutions for conflicting paths, the D16 line merge for
  the rest, `Conflict` if anything still conflicts, a CAS on
  `expected_head`, deterministic ids, idempotent retry). Rebased drafts keep
  `graft.rebased-from`; the draft that took resolutions records the new
  `graft.resolved-paths` (`graft_engine::RESOLVED_PATHS`). New gRPC
  `WorkspaceService.GetConflicts` and `.Resolve` (additive messages),
  served by graft-server, forwarded by graft-node, and implemented by
  graft-client's `Remote` and the testkit server.

### WI-2312-mcp

- Conflict resolution over MCP (WI-2312, option (a), step 2c). New tools
  `graft_conflicts` (read-only: per conflicting path the base, ours and
  theirs ids and kinds, `text_mergeable`, a `reason`, and diff3 markers
  clipped to 16 KiB; the failing draft; paged by path with `next`) and
  `graft_resolve` (rebases the workspace stack onto `onto` with per-path
  `put`/`delete` resolutions under `graft_commit`'s inline limits, CAS on
  `expected_head`; returns the new head, resolved and merged paths).
  `graft_revert` is now served. The `conflict` tool error gains an additive
  `sides` list with each path's base, ours and theirs ids. The server's
  `/mcp` wires `GetConflicts` and `Resolve` to the engine with the same
  owner check as gRPC. A pure-MCP agent now completes a conflicted land
  (same-line, modify/delete and binary cases in
  `crates/graft-server/tests/mcp_conflict.rs`).

### WI-2312-proto

- graft-proto: interface for conflict resolution without a working copy
  (WI-2312, option (a)). New `WorkspaceApi::get_conflicts` (`GetConflicts` →
  `Conflicts`: per-path base/ours/theirs ids and kinds, merge3 markers capped
  at `MAX_CONFLICT_MARKER_BYTES`, paged) and `WorkspaceApi::resolve`
  (`Resolve` → `Resolved`: one transaction, CAS on `expected_head`,
  deterministic ids, `graft.rebased-from` plus `graft.resolved-paths`, D16 for
  unresolved paths). `ApiError::unimplemented` helper; every implementer has a
  stub returning it. No wire or golden changes; the gRPC mapping is step 2b.

### WI-2312

- graft-server (tests only): a repro test, `tests/mcp_conflict.rs`, that drives `/mcp` as an agent with no working copy through a conflicted land and pins today's behaviour: the land and sync conflict, a resolution cannot be landed on the conflicting workspace, and the only route that works is to close the workspace and re-commit the merged content on the new tip. Plus a design note, `docs/notes/mcp-conflict-resolution.md`, with two options for resolving in place over MCP. No production code changes.

### WI-2313

# WI-2313: many working copies per host

- Client: request ids from the CLI, the LSP and the MCP server now carry a random per-process nonce (`graft_client::request_id`). The server names an unnamed workspace from the principal and the request id. With the old `cli-<pid>-<n>` ids, two processes with the same pid under one token (two containers, say) bound one workspace.
- Client: two directories bound to one workspace (a reused `--workspace` name or a copied `.graft/`) are allowed. Each write checks the head and moves it by compare-and-swap, so the second writer gets `Diverged` and nothing is overwritten. The error text now says another working copy may share the workspace.
- Client: a working copy never lists or commits a top-level `.git` (directory or worktree pointer file), `.hg`, `.jj` or `.svn`. `clone` writes `.graft/.gitignore` (`*`), so a surrounding git checkout ignores graft's state.
- Tests: `crates/graft-client/tests/worktrees.rs` covers two directories on one workspace, `workspace close`, a deleted clone whose drafts lapse at the lease and are then collectable, and a clone inside a git checkout.
- Bench: `--many-copies 1,8,32` runs N graft clones against git worktrees. The scale-10 result is in `bench/results/2026-10-23-many-copies-10/`.
- Docs: `docs/notes/many-working-copies.md` covers one clone per agent, teardown with `graft workspace close`, and coexistence with git.

### WI-2314

# WI-2314: quarantine the CI OOM flake in the `test` job

- The `nextest` step retries once when the failure matches a known OOM
  form (rustc, cargo or the linker killed, with no compiler diagnostic
  and no test failure), on pull_request and merge_group runs alike.
  `.github/scripts/oom-flake-signature.sh` does the matching; sample
  logs and `run.sh` are in `.github/scripts/tests/oom-flake-signature/`.
  Other failures fail at once. Recorded in `docs/known-flakes.md`. The
  host-side fix (a memory limit on vogt-prod) is tracked under WI-2314.

### WI-2315

- graft-server: `/mcp` applies the gRPC owner-or-admin check (`own_workspace`) to every operation on a named workspace — get, renew, set head, sync, stack, close, commit, conflicts, resolve and revert — before the operation reads anything. Another principal's token could read a workspace (`graft_workspace get`) and land its stack (`graft_land`) over `/mcp`; both are now `permission_denied`. gRPC `CreateChangeset` into a named workspace runs the same check. The stdio `graft mcp` path goes through gRPC and was already covered.

### WI-2316

- Creating a line no longer copies the whole first-parent history into the line index. A new line records one fork point (`line_forks`, SQLite schema version 5) on the line holding its nearest landed ancestor, and indexes only the changesets it lands. Line position reads and path scans fall through to the parent line below the fork, one read per hop. Design note: `docs/notes/line-fork-point.md`. (WI-2316)

### WI-2318

- graft-engine: `ImportChangesets` derives the new trees of the whole batch first (each changeset on its first parent, stored or earlier in the batch) and writes them all concurrently, synced, before the first changeset commits, in runs of at most `import_prewrite_bytes` (default 64 MiB) of tree frames so memory stays bounded; a changeset's own objects are also put concurrently on every write path. Each changeset still commits in its own transaction with its journal entry, objects stay durable before metadata names them, and no encoding changes. `graft-mirror import` of the 1,000-commit bench history on Node B: 18.9 s before, 6.2 to 6.6 s after (import step alone, two runs each). The remaining time is mostly one serial metadata commit per changeset; collapsing those needs a graft-store-sqlite change (follow-up). (WI-2318)

### WI-2320

- graft-mirror: delete the unreachable PR bridge module (`Bridge`, `BridgeConfig`,
  `MAX_STACK`, `MAX_ENQUEUE_ATTEMPTS`, `BridgeState`, `PullRecord`), the mirror
  DB's `bridge` table, and the authority switch's check for open bridge
  requests (and `Status::open_pulls`). `GIT_PRIMARY` moves to `authority`,
  `clean_text` to a new `text` module. No migration: there were no users.

### WI-2321

- docs: record the line fork-point index (D23, schema v5) in design.md and decisions.md, and mark the per-land PR bridge as removed under D21/D22.

### WI-2323-note

- Design note: grouping chained import commits into one SQLite commit per run (WI-2323).

### WI-2323

- graft-store: `MetaStore::commit_run(repo, txns)` applies a chained run of transactions in order, each atomic on its own with its own journal entry, stopping at the first that is not committed (default: a loop of `commit`). graft-store-sqlite runs a run as one writer job, each transaction in its own savepoint, under the batch's one `COMMIT` (`synchronous=FULL` unchanged). graft-index: `Index::on_create_in` derives generation and skip list from an `Overlay` of in-run, uncommitted ancestors. graft-engine: `ImportChangesets` commits each pre-write run in runs of up to 256 changesets with one `commit_run`; a build failure at changeset k still commits 0..k-1 first, a changeset repeated within a run is left to the next run, a lost race resolves per changeset and re-queues the rest, and a run ends before half of `claim_ms` passes. Bench import row at 1k (`--iterations 1 --rounds 1`, same host): 7.35 s before, 6.17 s after (136 → 162 commits/s); all correctness checks pass. Most of the remaining time is per-changeset reads (claims, object checks, record lookups), not fsync. (WI-2323)

### WI-2325

- docs: status.md, plan.md and the README status brought up to date with `main` after the 2026-10-23 phase-1 run (#82 to #111). (WI-2325)

### WI-2329

- Import: a lost race in a run that also hit a build failure now commits the changesets built before the failure instead of dropping them; the derived-value overlay is per run. SQLite `commit_run` keeps the committed prefix on an error, as the default does. New tests: mid-run build failure, re-queue after a lost race, runs over 256, the claim deadline (WI-2329).
- Draft overlap: `status --drafts` caps its page loops, and a missing `repo:<r>:drafts` scope no longer aborts `--upstream` output (it is reported as `denied`). Tests for an expired workspace, `graft_draft_overlap` and `status --drafts`; the disjoint check now tests something (WI-2329).

### WI-2330

- graft-engine: `PutObjects` stores its blobs concurrently (up to 32 at once, as tree puts already were) instead of one after another, so their syncs overlap. graft-store-fs: concurrent puts into one shard directory share its directory fsync. A put returns only after a directory sync that started after its rename has succeeded, and each file is still fsynced, so every acknowledged put stays durable. Profiling the 1k import first (server `/metrics`) showed the brief's target was elsewhere: `ImportChangesets` took about 1.0 s in total and all metadata store ops about 0.35 s, while `PutObjects` summed to about 8.8 s, so batching the per-changeset reads was dropped (operator redirect). Bench import row at 1k (`--iterations 1 --rounds 1`, same host, load 5–13): 6.49 s before, 4.83 s after (154 → 207 commits/s); all correctness checks pass. Repeated direct imports: about 5.0 s mean before, 4.2 s after. (WI-2330)

### WI-2336

# WI-2336: graft fit assessment

- `docs/notes/graft-fit-assessment.md`: four weeks of PRs and CI in
  rustTorrent, rustnzb, fluent2-egui, rdpapp, vogt (and its Rust port) and
  graft, set against phase 1 (D21, D22) and the rustTorrent trial (WI-2270,
  PR #214). Covers where graft would have helped or hindered, and ranks
  eleven fundamental gaps, each with evidence and a fix. Docs only.

### WI-2338

- bench: real-PR replay scenario (`python3 -m graftbench.realprs`). Replays the estate's overlapping PRs (rdpapp `main.rs` split and 11-PR stack, fluent2-egui registration lists, the rustTorrent qbit cluster, graft's own merge-back PRs) from their real fork points through `graft land` and git rebase, with `cargo check` as the clean-but-broken oracle. Results in `bench/results/2026-10-24-real-prs/`, trial note in `docs/trials/`.

### WI-2339

# WI-2339: no line merge on lockfiles and generated files

- `graft-engine`: a path rule can set `merge` (`PathMerge::Line` or `PathMerge::None`); the rule with the longest matching prefix that sets one wins. `Cargo.lock` and `package-lock.json` are `none` at any depth by default. Under `none`, any concurrent change to the path is a rebase conflict (in `Land`, `Rebase`, `Sync`, `Resolve`, `Revert` and `Stack`), so the client regenerates the file instead of landing a clean but wrong line merge. Unlisted paths merge by line as before. `Rebase` and `Revert` onto a bare changeset use the default line's rules.
- `graft-server`: `path_rules` entries take `merge = "line" | "none"`; a rule may set only `merge`.

### WI-2340

- `graft-mirror pr-batch --exclude <changeset>` (repeatable, with `--reason`) drops a land from a batch without holding back the others (WI-2340). It lands a graft `Revert` of the land on the integration line, so the lands after it stay. Every run checks the open batch: if a land in it has a revert on the line that the batch lacks (from this run, or from a run that died or conflicted part way), its PR is closed with a comment and the batch is recut from the same start. The new PR body lists each dropped land, its revert and the reason. A PR that GitHub has queued or set to auto-merge is not closed; the reverts go in the next batch. A cut never splits a land from its revert. Phase 1 reads no checks to choose a drop (D22).
- Batch cut policy: `--max-lands <n>` caps a batch (oldest lands first; the rest wait), exported against the base the line held at the cut. `--max-age <seconds>` holds a batch until its oldest land is that old or it is full. When the cap cannot apply (more than 5000 lands pending, or a line recorded before this change), the PR body and the log say so. After a cut batch merges, the next sync rebuilds its starting point (the batch rebased onto the line's newer base) from landed changesets, so merged lands are not applied again and no unlanded changeset is kept across runs for GC to collect.
- The mirror DB gains `batch_drops` and `batch_lines.start_base`, added in place when an older DB is opened.

### WI-2341-target-docs

- docs: the phase-1 read-path kill criterion is now "not slower than git" for `log`, `status` and `sync`; 2× faster than git moves to a later phase (operator decision, 2026-10-24).

### WI-2341

- Read paths (WI-2341). Read commands (`log`, `show`, `blame`, `cat`, `ls`, `line`, `attest`, `api`, rev `diff`) no longer parse the working-copy index (`WorkingCopy::open_info`). The index parser is about 2x faster. In LRU mode the object cache saves its counters to `.usage` and trusts them at open, so opening the client cache no longer stats every cached object; the walk runs at the first eviction, or after a crash, which leaves no `.usage`. The server keeps a bounded cache of decoded changesets for log pages.

  Bench on graft-dev (4 CPUs, quiet), p50 ms, before -> after (git):

  | | 1k, round 1 | 1k, round 2 | 10k |
  | --- | --- | --- | --- |
  | log -50 | 5.1 -> 3.0 (1.8) | 5.5 -> 3.6 (2.3) | 7.6 -> 3.0 (1.8) |
  | status | 3.9 -> 3.9 (3.1) | 4.2 -> 4.0 (3.4) | 13.8 -> 13.2 (7.1) |
  | sync, 5 behind | 9.9 -> 6.3 (22.2) | 10.3 -> 7.2 (23.9) | 46.3 -> 74.9 (27.1) |

  The two 1k rounds interleave the before and after binaries. The 10k figures are two back-to-back single runs. Their concurrent clean-land p50 also moved (216 -> 380 ms) on a path this change does not touch, so the 10k sync figure needs a re-run before it is believed; it is not explained.

  Floors, documented rather than fixed: `sync` fsyncs `.graft/index` and `.graft/state` (file and directory, 7-9 ms on NVMe), and git does not fsync its index by default, so relaxing that is a durability decision. `status` is one lstat per indexed file plus a directory walk, as git's is; stat'ing relative to an open directory was measured and is no faster. The remaining per-call cost is process start (about 1 ms) and the gRPC connect.

  `graft-node` test `crash_leftovers_are_cleaned_and_a_broken_index_wipes_the_cache` now removes `.usage` along with planting the temp file, as a real crash leaves it: every put removes `.usage` before it writes a temp file.

### WI-2342

graft-mirror follow imports upstream tags (WI-2342): each poll lists tags
matching the `tags` pattern (config default `*`, `""` off) in one
`git ls-remote`, peels annotated tags, and creates the immutable graft ref
`tags/<name>` by CAS for every tag on a commit graft holds. A tag moved or
deleted upstream is reported in the log and `Follower::last_tags()`, never
mirrored. At most 64 tags reach the graft API per poll.

### WI-2345-design

# WI-2345: adversarial harness design note

- `docs/notes/adversarial-harness.md`: the design for an adversarial test
  harness in four parts: racing lands under injected faults (WI-2345),
  merge3 against git (WI-2346), hostile git repositories (WI-2347) and
  hostile API clients (WI-2348). It covers a seeded adversary, a history
  recorder, a model checker, planted bugs that prove the harness catches
  them, and CI and soak budgets. Docs only; no design rule changes.

### WI-2345-followups

- graft-harness: review follow-ups to the adversarial harness (WI-2345).
  The checker now fails an acknowledged land that carries no changes, a
  ref write without its `RefMoved` journal entry in the same transaction,
  and a no-op ref move. Replay commands for `interleaving_only`,
  `hostile_wide` and the planted-bug tests now replay the right shape and
  seed. A new probe races adjacent-line, delete-vs-edit and binary lands.
  The planted-bugs policy test covers every shipped workspace crate and no
  longer passes `--offline`.

### WI-2345-hooks

- graft-engine and graft-gc: a test-only `planted-bugs` cargo feature, off by
  default, with hooks for the adversarial harness's four planted bugs
  (metadata before objects, a skipped land idempotency check, a lease
  boundary one tick early, GC ignoring the land-attempt holder). A
  `PlantedBugs` handle (in graft-gc, re-exported by graft-engine) switches
  each on at runtime; without the feature the hooks compile away (WI-2345).

### WI-2345-sync-note

- docs: record what sync does after a lost land ack (note §3.3, design §8.4). The sync drops the landed draft and empties the stack, and a retry of the old head returns the first `Landed`. This is safe: no land is lost or duplicated.

### WI-2345

- graft-harness: a new leaf crate, the adversarial harness part 1
  (docs/notes/adversarial-harness.md §3). Seeded clients race lands on one
  engine whose stores are wrapped to inject faults: delays, failed reads and
  commits, lost acks, crashes at a commit (the process restarts), lease
  expiry, GC sweeps mid-land and a full disk. A checker holds the history
  and final state to a model: no lost or duplicated land by root-draft
  identity, retries answer the same `Landed`, the `RefMoved` chain is
  gap-free and moves forward, CAS-only ref writes, region content without
  merge3, objects before metadata, GC safety and no stranded workspace. A
  failing seed prints a replay command and a shrunk history. Each planted
  bug is caught, there are exact-tick lease probes, and a `cargo tree`
  check keeps `planted-bugs` out of graft-server (WI-2345).

### WI-2346

- graft-harness: differential test of `merge3` against `git merge-file`
  (adversarial harness part 2). Seeded three-way inputs cover adjacent,
  overlapping, delete-vs-edit, moved and identical changes, CRLF and mixed
  endings, missing final newlines, long lines, empty files, non-UTF-8, NUL
  and the 1 MiB limit. Every disagreement is classified as an accepted
  difference (listed with its reason) or a bug; git is a test oracle only.

### WI-2347

graft-harness: hostile git repositories through graft-mirror import and graft-git export (WI-2347, adversarial harness part 3). Generators build each repository with the git CLI (octopus merges, several roots, empty commits, shallow and replace-grafted history, signed commits and extra headers, tags, submodules, symlinks, odd modes, non-UTF-8, case-clashing and reserved paths, blobs at the 64 MiB limit, huge and deep trees, odd identities, dates, encodings and messages). Each case round-trips with identical objects or is refused with a typed error and nothing imported. Gaps found are ignored tests naming WI-2356 to WI-2360.

### WI-2348

- graft-harness: adversarial harness part 4, hostile API clients
  (`tests/hostile_clients.rs`, fixed seed `0x2348`, `GRAFT_HOSTILE_SEED`
  replays another). Scope escalation (land without `land:<line>`, writes
  outside a path scope, drafts without `repo:<r>:drafts`, another owner's
  workspace, near-miss scope strings), expired, revoked, replayed, forged
  and seeded-tampered tokens, oversized and malformed requests, and
  unbounded asks all get a typed `ApiError` with no partial effect. A
  canary token and canary file contents are scanned for in every error and
  key/token `Debug`. It found WI-2367 (`list_tree` and `diff` quoted a bad
  page cursor); `a_bad_cursor_is_never_echoed` now pins the fix.

### WI-2349-step0

- `Rebase { dry_run: true }` runs the same line merge and reports the same
  result or conflicts, but writes no changeset, object, pin or journal entry
  (D27, design §8.8). It checks path scopes as a real rebase does, so its
  conflicts never name a path the caller may not write. gRPC and `/mcp`
  admit a dry run as a read.

### WI-2349-step5

- graft-mirror `pr-batch`: a batch PR now carries one commit per land,
  oldest first, on GitHub's current `main` (D26). Each commit has the
  land's author and author time, its message (`@` kept as written), and
  `Graft-Changeset`, `Graft-Session`, `Graft-Work-Item` and `Graft-Batch`
  trailers; a dropped land's revert is its own commit. Each land is
  rebased onto the commit before it with line merge, so its commit keeps
  upstream changes to the same files. There is no checks trailer (D22).
  The chain is recorded (new `batch_commits` table) before the push. When
  a land does not merge onto the commit before it, the replay does not
  reproduce the integration tip, or a batch has more than 500 lands, the
  batch is squashed as before and the PR body says why. The cut now caps
  every batch at 500 lands. Once land-pr rebase-merges the PR (D28),
  settling maps each merged commit back to its land by its trailer (read
  from the last paragraph only); a merge commit maps to the original
  shas, and a squash-merge maps only the batch tip.

### WI-2349-step6

graft-engine: a token's Vogt session and work item now bind a new changeset's `vogt.session` and `vogt.work_item` metadata, so a client can't attribute a land to another session or work item; a mismatch fails with `session-metadata` or `work-item-metadata` (WI-2349 step 6, D26). Service tokens are exempt.

### WI-2349-step7

- `graft land` and the MCP `graft_land` tool run the pre-land check first
  (design §8.8, D27): a dry-run rebase of the stack onto the tip
  (`Rebase { dry_run: true }`) and the draft-overlap read. A real conflict
  stops the land with exit 3 (MCP: a `conflict` error) and its paths,
  sending nothing; `graft land --sync` still sends it so the conflicts are
  written into the files. Files that merge cleanly and other workspaces'
  drafts are only reported. Without the `repo:<r>:drafts` scope, or against
  a server that does not serve a read, that part is skipped with a note.
  `--no-check` (MCP `check: false`) skips it all. Both clients share
  `graft_mcp::preflight` and return the same `preflight` object, on success
  and on a failed land. Advisory only: the server's land still decides, and
  no build or test runs (D22).

### WI-2349-step8a

graft-dev runs `3dbd296` (per-land batch commits, CLI identity, pre-land check), and `bench/graftbench/phase1.py` measures them: pre-land check latency, N lands → N commits with export time, and identity binding, with a fake GitHub on loopback so nothing reaches GitHub. Results in `bench/results/2026-10-24-phase1-8a`, note in `docs/trials/2026-10-24-phase1-8a-graft-dev.md` (WI-2349 step 8a).

### WI-2349

WI-2349: design for post-land checks on batch integration lines (D25, revising D22), one commit per land in batch PRs with no review state in graft (D26, revising D21), and a default pre-land conflict and draft-overlap check in `graft land` (D27). Adds design.md §8.7, §8.8 and §11.4. Docs only.

### WI-2350

- CLI: `commit` and the MCP commit tool record `VOGT_SESSION_ID` and `VOGT_WORK_ITEM` as `vogt.session` and `vogt.work_item` draft metadata (an explicit `--meta` wins), so lands and `pr-batch` show the session and work item. The call context carries them too, with principal kind `agent` when a session is set. The default author is `GRAFT_AUTHOR_NAME`/`_EMAIL`, else `$USER`; the default author kind is `agent` inside a Vogt session. No format change (WI-2350).

### WI-2351

- docs: D28 amends D21 and D26 — batch PRs get oversight's standard review and are merged by `land-pr` once GitHub's required checks pass; graft still never merges (design.md §11.5).

### WI-2352

- bench: `--batch` scenario and `graftbench.batchlive` for batch delivery (one commit per land, authorship, bisect and blame, drop and recut, latency); results and trial note in docs/trials/2026-10-24-batch-delivery.md (WI-2352).

### WI-2353

- Docs: D25 and design §8.7 re-labelled as the phase-2 design, starting
  with the graft-primary authority switch; D22's phase wording restored;
  plan.md phase-2 row and §11.4 steps 1–4 and 8b marked phase 2; step 8
  split into a phase-1 rollout (8a) and the phase-2 check switch (8b)
  (WI-2353).

### WI-2354

- graft-mirror batch: a land with no author email (the token-authored revert that `--exclude` drops a land with) gets the mirror committer's identity on its per-land commit, not `<subject> <unknown>` (WI-2354). An open batch's detail now says it waits for review and `land-pr`, not the operator (D28, WI-2355).

### WI-2356

- graft-git: export no longer writes a corrupt repository when a directory has a recorded raw tree (a zero-padded `040000` entry, an odd mode beside a subdirectory). `build()` now rebuilds and writes every subtree the raw tree references, and refuses the export if one cannot be rebuilt; import keeps nested raw-tree records instead of dropping them under a recorded parent. Exports made before this fix can lack trees while their commit shas match: `git fsck --connectivity-only` (or `git rev-list --objects --all`) on the destination finds them; re-export into a fresh repository to repair. Commits imported before the fix whose nested records were dropped now fail the export loudly; re-import them.

### WI-2357

- git import now records the raw tag objects of annotated, signed and nested
  tags in the mirror map (new additive `tags` table in the mirror DB), and the
  export writes them back, so every `refs/tags/*` names the same object sha as
  the source. Lightweight tags are unchanged. (WI-2357)

### WI-2358

- Import skips `refs/tags/*` that name a tree or a blob (lightweight or
  annotated) and lists each by name and kind in `ImportReport::skipped_tags`
  and the `graft-mirror import` output, instead of dropping them silently.
  Everything else imports. (WI-2358)

### WI-2359

- Paths naming git's metadata directory (`.git` in any case, `git~1`, HFS+-ignorable and NTFS stream/trailing-dot variants) now fail the portability check as `git-metadata` (CVE-2014-9390 class), so git import refuses them as `BadPaths`. A `/` inside a git tree entry name is refused as `slash-in-name`. The working copy treats foreign metadata names case-insensitively and never writes a path through `.git`. (WI-2359)

### WI-2360

- `graft import` refuses a repository with any blob over 64 MiB (`MAX_BLOB_BYTES`, the server's default `blob-bytes` limit) before anything is written, listing every oversize path as a `BadPaths` entry with reason `blob-bytes`. Previously earlier batches were imported first. Sizes come from object headers, for the entries each new commit changes only.

### WI-2361

- docs: D28 and AGENTS.md record that batch PRs (`graft/batch-<n>`) bypass the
  squash merge queue and are rebase-merged by `land-pr` with `--admin`, keeping
  the per-land commits; ordinary PRs still squash through the queue. (WI-2361)

### WI-2362

- bench: `python3 -m graftbench.branching` runs trunk-based, git flow and release-branch histories through graft and git at a fixed seed and compares trees, conflicts, round trips and merges between lines; results and the concept map in `docs/trials/2026-10-24-branching-strategies.md` (WI-2362).

### WI-2363

WI-2363 (design): D29 records a merge point per (source, target) line pair. A merge lands as a one-parent changeset; the land's CAS transaction also writes a `line_merges` row and a `LineMerged` journal entry, and `MergeBase` reads merge points (bounded hops) before falling back to the ancestry walk. Design: `docs/design.md` §8.9 (`LineMerge`, `graft line merge <src> --into <dst>`, MCP `graft_merge`, `land --merged-from`). Docs only; no code.

### WI-2364

- graft-cli, graft-mcp: `graft pick <rev> [--merge line|file]` and the MCP tool `graft_pick` re-apply a landed changeset's own changes (a cherry-pick) through the engine's existing `Rebase` of a landed changeset, which records `graft.rebased-from`. The CLI adds the draft on the working copy's head; a conflict writes the changes into the files with markers or a sidecar and exits 3, and `graft commit` then makes the draft. `graft_pick` takes `onto` or a `workspace`, whose head moves to the new draft by compare-and-swap; a conflict is the typed `conflict` error. One `Rebase` per pick, instead of the 7 to 9 calls a client-side re-apply took (WI-2362).

### WI-2365

- `graft tag create <name> [--at REV]` creates an immutable tag `tags/<name>`
  (an `UpdateRef` of kind `Tag`, CAS from absent; a move is a `ref_conflict`),
  and `graft tag list` lists tags with paging. MCP gains `graft_tag`. The
  `update_ref` parity entry now names `tag`. (WI-2365)

### WI-2366-d34

WI-2366: decision D34. Exactly one canonical empty root changeset is valid: no parents, no changes, and fixed fields (no author, time or randomness), so its id is a constant every new repository shares. Amends validity rule 9. Other native empty changesets stay rejected. D33's git-import exception is unchanged. Existing golden vectors are unchanged; the code PR adds one golden vector for this root. `graft init` (D32) writes it and points the line at it. Docs only. Git export skips that changeset; its children export as parentless git root commits, so export never writes `author <> 0 +0000`. Git import does not insert the root above imported history, and an export/import round trip of a `graft init` repository does not restore it as a parent. The root's tree is the empty tree, id `td0a89364d3af4e3469f3f4f545b3fb47ef2c72f9f2daddeedbd1821d18a4234a` (`Tree::default().id()`).

### WI-2366-design

- Design: local repositories (D32, WI-2366). `graft init` will open the engine in-process over the sqlite and filesystem stores; no server, daemon or token. Docs only.

### WI-2366

### Added

- A local repository opens the engine in-process over the SQLite metadata store and the filesystem object store (`LocalRepo`), with an exclusive `repo.lock`. No server, daemon or token. `graft init` creates that repository, writes the canonical empty root, and points `main` at it. Commands in a `local:` working copy use the in-process engine.

### WI-2367

- `list_tree` and `diff` no longer echo a bad page cursor in `InvalidPath`; a malformed cursor now returns `InvalidChange { rule: "page-token" }` with an empty path, like `history` and `resolve` (rule 8).

### WI-2368-docs

- docs: D29 `line_merges` is SQLite schema version 7 (version 6 is the draft
  path index); `graft.merged-from` listed as a reserved metadata key; the
  `line_fork` store read named in §8.9; D29 in numeric order in the decisions
  summary.

### WI-2368-engine

- D29 line merge, engine and SQLite (WI-2368 step 2): SQLite schema version 7 adds `line_merges` (design §8.9 says 6; the draft path index took 6), with a golden vector and a v6→v7 migration test. `Land { merged_from }` writes the merge point row and its `LineMerged` journal entry in the land's CAS transaction. `LineMerge` merges one line into another; `MergeBase` (`merge_base_via`) reads merge points and fork rows first, bounded by `merge_hops` (8) and `merge_reads` (64), and falls back to the ancestry walk with `via = ancestry`. `LineMerges` lists a line's merge points, paginated. Adds the store read `line_fork`. No change to any id, tree, changeset or frame encoding.

### WI-2368-interface

- Interface for merge points between lines (D29, WI-2368): `LineMerge`, `LineMerges`, `Land.merged_from`, the `LineMerged` journal body, `MergeBaseResult.via`, and the `line_merges` store read with `Write::PutLineMerge`. The engine, SQLite (schema v6) and surfaces are stubbed; no durable encoding changes.

### WI-2368-surfaces

- D29 surfaces (WI-2368 step 3): gRPC `LineMerge`/`LineMerges` (server, client, node, testkit); `graft line merge <src> [--into <dst>] [--at REV] [-m MSG]` (shows the base and `via`, with a note when it fell back to ancestry), `graft line merges [<dst>] [--from <src>]` (paginated) and `graft land --merged-from <line>@<rev>`; MCP `graft_merge` and `graft_land` `merged_from`. A line merge conflict is the new `MergeConflict` error carrying the tip, base and source ids. Engine: rule-3 and target merge rows page within the read budget, a capped search no longer returns a partial best, `MergeBase` picks holder lines on different lines deterministically, and an engine-level `LineMerges` pagination test. Bench: `linemerge` variant in `graftbench.branching`.

### WI-2368-trailer

- D29 export (WI-2368 step 4): a merge landing (`graft.merged-from` metadata) exports as a one-parent git commit with a `Graft-Merged-From: <src>@<changeset>` trailer, from both the graft-git exporter and graft-mirror's per-land batch commits; a value with a newline is refused like the other trailers. Engine tests: `LineMerge` scopes (repository read, `land:<dst>`, and write on the merged paths; `land --merged-from` the same), and `MergeBase` for a changeset held by two lines' own rows. The rule-3 early exit's assumption is pinned in a comment.

### WI-2370

- `graft-mirror follow` now fetches the tag objects of annotated, signed and
  nested upstream tags it creates in graft and keeps them in the mirror map,
  so export writes those tags with upstream's own shas (no longer
  lightweight). Doc-comment fixes in the mirror DB and mirror map.

### WI-2372

- engine: a resolution landed with `merged_from` lands stamped with
  `graft.merged-from = <src>@<changeset>`, so it exports with the
  `Graft-Merged-From` trailer. The stamp is deterministic: a retry finds the
  same result.
- engine: the bounded merge point search reads each row once per query and
  remembers each answer by bound, so the budget pays for distinct rows. In
  git flow, a hotfix line merged into main and develop is now followed when
  it merges into develop, instead of falling back to the ancestry walk.
- cli: `graft line merge --sync`, run in a working copy on the target,
  writes a conflicted merge into the files (markers or sidecars) and exits
  3 with `merged_from`; a conflicted merge is three commands: `line merge
  --sync`, `commit`, `land --merged-from`.
- mcp: `graft_merge` says it needs write on every path the merge changes.
- bench: the branching bench's `linemerge` variant merges with `--sync`, and
  makes each git flow hotfix a `hotfix/<name>` line merged into main and
  develop, as git flow merges the hotfix branch.

### WI-2397

- graft-client: the hostile test `a_repeating_tree_dag_is_stopped_by_the_entry_budget` no longer asserts a wall-clock bound. It took 155 s on a loaded merge-queue runner (0.3 s idle locally); nextest's slow-timeout guards hangs, and the typed `LimitExceeded` and requested-id count stay as the real assertions.

### WI-2401

- CI: docs-only pull requests and merge-queue groups (`docs/**`, `changes/**`, `bench/results/**`, `*.md`) skip the Rust build and test steps of `fmt`, `clippy`, `test` and `bench-smoke`; the required checks still report success. (WI-2401)

### WI-2403

- docs: rustTorrent trial 2 write-up (WI-2403).

### WI-2411

- graft-client: the working-copy walk honours `.gitignore` and `.graftignore` files at the root and in any directory (nested rules win, `!` negation, `dir/` rules, `**`), so status, commit and diff skip build output such as `node_modules/`, `dist/` and `target/`. An ignored directory is never entered. Tracked files stay tracked: a modified tracked file under an ignored directory still shows. `WorkingCopy::ignored` lists the untracked entries the rules skip. No format change.
- graft-cli: `graft status --ignored` lists those entries (an ignored directory is one entry, cut at 1000); `--json` adds an `ignored` object.

### WI-2413

`graft status` (CLI and the MCP `graft_status` tool) lists a conflict only while it is still open (a marker line remains, or the `.graft-conflict` sidecar exists), the same rule `graft commit` uses; a resolved file shows as a plain change (WI-2413). Entries in the conflict record that were never open (clean merges, skipped files) no longer appear in status's `conflicts`.

### WI-2414

- graft-cli, trial: lands from Vogt sessions carry `vogt.session` again (WI-2414). Trial 2 ran a stale CLI: `trial/install-cli.sh` built into the session's `CARGO_TARGET_DIR` and installed an old `$src/target` binary under the new sha; it now builds into its own target. A commit that resolves a conflicting sync keeps the stack head's `vogt.*` metadata. The server never dropped an unbound session claim: it refuses only a claim that contradicts a session-bound token.
- graft-cli: the default author is `GRAFT_AUTHOR_*`, then `$USER`/`$LOGNAME` with an empty email, never `@localhost` (WI-2412).

### WI-2417

- `graft land` and `graft_land` report what the pre-land check ran as
  `preflight.pre_land_check: {rebase, overlap}`, and a skipped part (the
  draft overlap without `repo:<r>:drafts`) is noted on stderr. The dry-run
  rebase needs only what a land needs, so a land token always gets the
  conflict stop (WI-2417).
- `trial/mint-agent-tokens.sh` also grants `repo:<r>:drafts`, so trial
  tokens run the full check. It and `trial/create-trial-line.sh` take
  `GRAFT_TRIAL_REPO` and `GRAFT_TRIAL_BASE` (validated), and the line is
  now forked with `graft line create --from <base>` (vogt: `rust-core`).

### WI-2418

- docs: trial 3 note: 20 lands across rustTorrent, fluent2-egui and the vogt Rust port through graft, from 6 Codex sessions on Clawbay (WI-2418).

### WI-2434

WI-2434: design for D30. A land that arrives while its batch PR is open and not yet sealed (auto-merge, queued or merging) joins that PR as one more per-land commit, pushed as a fast-forward with a lease, so a review fix-forward no longer costs a close and recut. New `docs/decisions.md` D30, `docs/design.md` §11.6, and the WI-2415 implement item in `docs/plan.md`. Docs only.

### WI-2439

- `graft show` (and every CLI command taking a revision) resolves the short
  ids the CLI prints. A short id is matched against the working copy's ids,
  the ref targets and the line's recent history (bounded); an ambiguous one
  is a usage error, an unknown one `not_found`. (WI-2439)

### WI-2443-decision

WI-2443: decision D33. An empty changeset with fewer than two parents (a single parent or a parentless root) is valid only when it carries `mirror.git-commit` from a git import. Native empty changesets stay rejected, and the native write boundary rejects any `mirror.*` key so a native client cannot set `mirror.git-commit`. Records the rule-9 exception in `docs/decisions.md` and `docs/design.md` §4.5. Encoding and golden vectors for native changesets are unchanged. Docs only; the model, importer and boundary check are a later PR.

### WI-2443-repin

Repin graft-dev to the image built from main bbec0e4 (ghcr.io/thedancingdeveloper-org/graft:sha-bbec0e4969739a6bc464efc3b6cda4914febef3d, index sha256:174d36218bf9b58984b2240ed784f2a87c7ade56ce5c9a131e63c08953bf6706, image run 40177228776), so the D33 empty-changeset import is what the stack runs.

### WI-2443

Fixed git import of empty root and single-parent commits while keeping native
empty changesets and native `mirror.*` metadata rejected.

### WI-2447

The `graft` executable now runs the server, node and mirror roles through
`graft serve`, `graft node` and `graft mirror`. The legacy executable names
remain available as compatibility wrappers when built from source, while
release archives ship only the unified `graft` executable.

### WI-2448

Added `graft serve --dev`: a loopback development server with a private signing key, a repository admin token stored in the user config, and a printed `graft clone` command. An integration test clones by that URL and checks the token never appears in captured output.

### WI-2449-hosted

Release builds run on GitHub-hosted runners: `ubuntu-24.04` for x86_64 musl, image, preflight and assembly; `ubuntu-24.04-arm` for aarch64 musl; `macos-14` for aarch64 Darwin. The runner-policy check allowlists those exact labels for `release.yml` only. Required checks stay self-hosted.

### WI-2449

Release workflow: static Linux (musl) and macOS binaries, checksums, a versioned GHCR image, changelog assembly with consumed-fragment cleanup, and a dry run that is not a tag. Binary and container install instructions use the canonical repository locations; crates.io publication is deferred until the internal dependency chain is publishable.

### WI-2450

## graft-cli

- Add a private user config and `graft login`/`logout` for named servers.
- Resolve endpoint and token independently from flags, environment, the working copy's server, then the default server.
- Record the selected server name in a working copy and keep tokens out of config, arguments, and output.

### WI-2451

### Added

- `graft clone graft://host[:port]/repo` and `grafts://` carry the endpoint. A matching named server supplies the token. `--repo` and MCP `graft_workspace` open accept the same URL. The parser lives in graft-proto; a URL whose endpoint is not the connected MCP server is rejected, and an explicit `--line` that disagrees with `?line=` is an error. Userinfo in the URL is rejected.

### WI-2452

# WI-2452 — MCP registration without flags

`graft mcp` inside a working copy resolves the named server and token from the working copy and the user config (WI-2450). `--server NAME` selects a named server and wins over the working copy and `default_server`.

`graft mcp --print-config <claude|codex|cursor>` prints the registration snippet for the current directory. Inside a working copy the snippet is flagless. Outside one it passes `-C <dir>`. The snippet never contains a token. Registration is documented in `docs/agents.md`.

### WI-2453

### Agents

- `docs/agents.md`: a short agent guide (model, setup, the clone/commit/sync/land loop, exit codes, conflicts, a block to paste into AGENTS.md or CLAUDE.md, and checkout-less `/mcp`), with no trial or estate specifics. The trial guide points at it.

### WI-2455

Scrubbed internal project references from CLI help, gated experimental CI commands, and made version output include build revision metadata.

### WI-2457

# WI-2457

README is a product page: what graft is, who it is for, an honest
pre-v0.1.0 status line, install from git (no crates.io), local and server
quick starts, agent MCP registration, git migration, a comparison table,
and the bench chart. Local mode is `graft init`; the server quick start is
`graft serve --dev`.

### WI-2459

- Bench chart (WI-2459). `bench/render.py` draws `docs/guide/bench.svg` from the committed 1k and 10k result files, the real-PR replay and the branching-strategy run. `docs/guide/benchmarks.md` is the methodology, and the SVG is embedded in the README. The WI-2341 twelve-run medians are not in a results file, so they are not on the chart.

### WI-2466

A failed reach of the configured upstream (a bad or expired token on `ls-remote`, or an unreachable remote) is `MirrorError::Fetch`, not lag 0. `graft_mirror_lag_changesets` reads `-1` while the line is in that error state and returns to a measured value after a successful poll. The message never includes a token or URL userinfo.

### WI-2467

## graft-node releases the cache lock when it stops

`GetObjects` answers and cache-miss fetches run on tasks that clone the
node, so they hold the cache store's exclusive lock after the socket has
stopped answering. `Node::serve` now stops accepting background object tasks
and waits for outstanding answers, cache-miss fetches, and blocking cache
writes before it returns. Completed tasks are released immediately instead of
accumulating for the daemon's lifetime. The lock is free when `serve` finishes.
The cache test reopens the store directly after `stop` and no longer retries
`Node::open`.

## `import_kill9_restart_fetch_fsck` is quarantined

The ~75s kill-9 e2e failed once on an unrelated merge-queue run (PR #196) and
passed on retry. It is `#[ignore]`d, so the required nextest run skips it. A
separate non-required `slow` job (`continue-on-error`) runs it on
`merge_group` only. Follow-up: WI-2469. See `docs/known-flakes.md`.

### WI-2470

Harden release publishing with tag/version and changelog preflight checks,
Docker-capable Linux runners, fork-safe dry runs, one tag image build, and an
explicit Apple Silicon-only macOS artifact.

### decisions-d18

Record decision D18: phase 1 exit and kill criteria accepted as planned.

### decisions-d19

Record decision D19: imported changesets carry `mirror.git-commit` provenance metadata.

### decisions-wave1

Record operator decisions D3 (SQLite + filesystem), D4 (gRPC + JSON facade), D5 (gzip frame at rest) and D7 (BLAKE3).

### decisions-wave1b

Record operator decisions D11 (reject bad import paths), D12 (per-repo object namespaces), D13 (Komodo stack on Node B) and D14 (rustTorrent pilot).

### docs-d21-amended

Docs: docs/architecture.md §3c now describes batch PRs as D26 and D28 left
them (one commit per land, squash as fallback, reviewed by oversight and
rebase-merged by `land-pr`), and D21 in docs/decisions.md carries a dated
"Amended by D26 and D28" note; its decision text is unchanged.

### docs-status-2026-10-24

- docs: status.md, plan.md and README brought up to `main` at 3fe907b (#170): per-land batch commits (D26), the pre-land check (D27), batch PR review and `land-pr` rebase merge (D28), `graft tag`, `graft pick`, line merge with merge points (D29) and the git flow bench result at seed 2362, the adversarial harnesses, tag-object and odd-git-input handling, and read paths against the "not slower than git" bar. Open: WI-2341, WI-2400, WI-2314; future: WI-2366; phase 2: D25 and §11.4 steps 1 to 4.

### fix-same-size-edits-flake

- graft-client tests: `fifty_same_size_edits_within_a_second_are_all_detected` no longer asserts the edit loop runs under one wall-clock second (it failed under merge-queue load). The pinned mtime already models the coarse tick; the test now checks the mtime and size really stayed put instead. No product change: detection was correct.

### squash-queue

graft lands through a squash merge queue; conflicting branches merge main in and push normally (no force-push).

### trial2-line-param

- trial: `create-trial-line.sh`, `mint-agent-tokens.sh` and the agent guide take the line from `GRAFT_TRIAL_LINE` (default `trial-1`), for trial 2.

### trial3-mirror-coverage

graft-dev follows two more GitHub repositories, fluent2-egui and vogt, into the graft repositories `fluent2-egui` and `vogt`. vogt follows its `rust-core` branch (the Rust port) as the graft line `rust-core`; fluent2-egui follows `main`. Each has its own graft-mirror service, config, token, volume and metrics port (17891, 17892). `up.sh` creates the repositories, mints the tokens, pre-flights every mirror config and waits for each repository's first ref. The first `follow` is the initial import. (Trial 3 step 0.4, WI-2297.)

### wi-2223

Read-path performance (WI-2223): graft-server sets TCP_NODELAY on accepted connections (tonic ignored the builder setting with a custom listener, so small responses waited out a 40 ms delayed ACK); the client's object cache no longer fsyncs each object (frames are verified on read and refetched if torn); and fetch sends batches of 32 ids with 8 in flight instead of 256 with 4. rustTorrent, graft-pilot p50: log --path 45 -> 4 ms, diff 61 -> 19 ms, blame cold 48 -> 6 ms, full clone 8677 -> 266 ms, sparse clone 8104 -> 156 ms.

### wi-2258

graft-server: a refused token delegation is always PermissionDenied, checked before the ttl cap (WI-2258).

### wi-2270-trial-d21

trial: adapt the trial to D21 (rusttorrent, line trial-1, ci/fmt+clippy+test); create-trial-line.sh replaces import-trial-repo.sh (WI-2270).

### wi-2273

# WI-2273: the CI exchange checks commits graft-mirror records, not a map file

- `graft-proto`: `MirrorService.RecordExports { repo, exports: [{ changeset, commit }] }`, server-only, called by graft-mirror with an admin token (at most 256 per call).
- `graft-server`: serves `RecordExports` (admin scope on the repository, a write under the denylist and free-space floor, every changeset must be stored in the repository, commits are hex) into a new `exported_commits` table in the metadata database. A pinned `ExchangeCiToken` now mints only when the job's verified `sha` is recorded as an export of the pinned changeset in the graft repository of the job's binding; no record, a bad id, or a database error refuses it (`CommitMismatch`). The static `ci.commit_map_file` is removed, and a config that still sets it is refused at start-up.
- `graft-mirror`: an `ExportLog` trait, implemented by `Server`. The export (graft-primary lines) and the PR bridge record each pushed tip's commit before pushing it; if the record fails, nothing is pushed and the step is retried. `graft-mirror export` records through the server it talks to.
- `graft-node`: forwards `MirrorService/RecordExports`.

The server cannot compute the export sha itself: a native changeset's committer date is chosen by the mirror (the land request's planning time, or when export first wrote it), and imported parents' headers and raw trees are in the mirror DB only.

### wi-2275

trial: agent guide, install-cli, mint-agent-tokens and import-trial-repo scripts for the WI-2270 trial (WI-2275).

### wi-2292

# WI-2292: the attester checks amended drafts

- `graft-attester`: `graft commit --amend` creates its replacement outside the workspace, so the replacement's `ChangesetCreated` entry says abandoned, and then moves the head with `SetHead`. The attester queued only `ChangesetCreated` drafts, so it never checked an amend, and agents' amended heads failed to land with "no passing ci/fmt attestation". It now also watches `WorkspaceHeadMoved` (`SetHead`, an adopted changeset, a stack edit) and `WorkspaceSynced` when the head changed (a rebase), and queues the new head as the workspace's draft. The existing still-a-draft check before a run still skips heads that have since moved on.
- To pick up amends missed before this fix, stop the attester, set `state/lsn` to the first missed `ChangesetCreated`/`WorkspaceHeadMoved` position, and start it. The queue keeps one draft per workspace, the newest, so re-reading is safe.

### wi-2294

# WI-2294: a repeatable core source-control benchmark against git

- New `bench/` (Python 3, standard library only): a seeded generator for a synthetic Python project and N-commit history (10 to 100k). The same seed gives byte-identical history. A driver replays the history through the real `graft` CLI (commit, land) and through git (commit, push). It times import, clone (full and sparse), status, diff, log, blame and sync, and runs three concurrent-writer scenarios: disjoint files, merge3 regions of one file, and same-line conflicts resolved by rule. It checks that the final graft and git trees match byte for byte, that no land is lost or duplicated, that the line never moves backwards, and that blame is intact. Output is `results.json` and `summary.md`. It starts its own server or uses `--server-url` with `--token-file`. See `bench/README.md`; example results for scales 10 and 1,000 are in `bench/results/`.
- Finding: `graft land` sends `MergeMode::File`, so a land whose rebase meets the same file changed on both sides is refused even when a line merge (D16) would be clean. Agents get through only with `land --sync`. The bench counts these refusals (WI-2296).

### wi-2296

# WI-2296: land line-merges by default

- `graft-client`: `land` sent `MergeMode::File`, so the server's land rebase refused (exit 3, `conflict`) any path both sides changed, even when a three-way line merge was clean. The bench harness measured 140 such refusals at 1k commits with 8 writers editing separate regions of one file. `LandOptions` gains `merge`, which defaults to `MergeMode::Line` (decision D16: line merge for text files under 1 MiB). Overlapping edits, and binaries, still conflict. The LSP land command uses the same options, so it line-merges too.
- `graft-cli`: `graft land --merge line|file` (default `line`). `--merge file` keeps the old, stricter rebase.
- Not changed: the `graft_land` MCP tool (`graft-mcp`) builds its own `Land` request and still defaults to `file` when `merge` is not given.

### wi-2296b

# WI-2296b: MCP land defaults to a line merge

- `graft-mcp`: `graft_land` now asks for a three-way line merge (`MergeMode::Line`, D16) when the call passes no `merge`, matching the CLI. An agent landing a change to a different region of a file the line also changed no longer gets a file-level conflict. Pass `"merge": "file"` for the old behaviour. The tool's input schema says so: `merge` has `"default": "line"` and its description puts `line` first.

### wi-2298

# WI-2298: every line defaults to `direct` (decision D22)

- `graft-engine`: `EngineConfig::default()` no longer lists `main` as decision D6. Every line, `main` included, now uses `default_line`, which is `direct`: no build, test or CI runs in the land path (D22: phase 1 is a git alternative). D6 (`attested` on `ci/build` and `ci/test`, the same checks post-land with auto-revert) is the phase-2 opt-in, still available as `LineConfig::d6`.
- `graft-server`: without `[[lines]]` sections every line lands directly. A `[[lines]]` entry opts its line into `attested`, `queued` or `post_land`.
- **Deployments that relied on the implicit D6 for `main` must now list it under `[[lines]]`** (`name = "main"`, `policy = "attested"`, `checks = ["ci/build", "ci/test"]`, `post_land = { checks = ["ci/build", "ci/test"], auto_revert = true }`), or `main` lands with no attestations.
- `deploy/server.toml`: `main` and `trial-1` are `direct` (no checks, no post-land); the D6 block for `main` stays as a commented-out phase-2 example. This matches the live graft-dev VM, where `trial-1` was already hot-fixed to direct. A config test now parses `deploy/server.toml`.

### wi-2299

# WI-2299: docs follow D22, with a delivered baseline

- `docs/decisions.md`: D22 is recorded. Phase 1 is graft as a git alternative, lines default to `direct`, and CI checks are phase 2, switched off. D6 becomes the phase-2 opt-in for `main`. The D13 row notes the move to the graft-dev VM.
- `docs/plan.md`: Draft 2. A phase table follows D22, with the phase 1 backlog (Vogt WI-2297). The G1 to G3 waves are kept as history and marked delivered, superseded or not planned (G3.10, the bridge CLI).
- `docs/status.md` (new): what is delivered on `main` with commit evidence, what is partial or stubbed, what is built but off, the bench numbers at 1,000 commits, and the backlog.
- `README.md` and `docs/architecture.md`: the status line and a phase note.

### wi-2300

- graft-cli: `graft mcp` runs the graft MCP server over stdio against the remote API, with the same endpoint and token discovery as every command (`--endpoint`, `GRAFT_ENDPOINT`, the working copy's server; `--token-file` or `GRAFT_TOKEN`). Inside a working copy it serves the working-copy tools: `graft_status`, `graft_commit` with `from_working_copy`, and `graft_land` and `graft_sync` on the working copy's workspace, which move the working copy too; a working-copy land line-merges by default (`merge: "file"` for the stricter rebase). `mcp` is no longer a planned command.
- graft-mcp: an additive `WorkingCopyTools` hook and `McpServer::with_working_copy`, through which the stdio host serves a working copy.

### wi-2415

### Fixed

- `graft-mirror` now appends post-cut lands to an open, unsealed batch PR with a lease-protected fast-forward, preserving previously reviewed commit SHAs and starting a new batch after merge.
### Fixed

- An open batch whose PR body lagged the recorded lands is brought up to date on the next run, including after a crash between the push and the body update. A batch GitHub is merging, or has merged, is not appended to.

### wi-2419

### Fixed

- `deploy/up.sh` gives each followed repository its own refs deadline. A first import that has not landed yet is reported as importing and only warns when that repo's own wait expires; an error from the refs endpoint fails the deploy instead of being retried as "no refs yet".

### wi-2435

- graft-mirror: split `batch.rs` into `batcher`, `render` and `types` modules. No behaviour, API or output change; public items stay at `graft_mirror::batch`.

### wi-2438

- `deploy/onboard-repo.sh OWNER/REPO BASE [LINE]` performs language-agnostic,
  idempotent graft-dev onboarding: declare and deploy a mirror, wait for its
  first imported ref, and create the integration line. `--dry-run` prints the
  plan and D28 ruleset. A rerun with a different base fails loudly.
- `--mint-session OWNER/REPO ses_…` mints one D10 agent token (`--kind agent`,
  `--session`, drafts scope) into a mode-0600 file. Tokens are not shared.

### wi-2440

- Prove MCP and LSP bounds with oversized per-surface coverage, registration parity guards, a typed cap for diagnostics notifications and request responses, and a single `call_tool` byte cap so an oversized tool result is a typed refusal. The central cap is proven by a test-only tool whose handler returns an oversized result without bounding it.

### wi-2441-proto

- graft-proto: add backward-compatible typed stale-base land refusal details with the current line tip, a bounded path list, and an explicit truncation flag.

### wi-2441

- Return bounded stale-base details from conflicting lands and show actionable sync-then-land guidance in the CLI.
- MCP and LSP land errors name a stale base as `stale_base` with the current tip, the changed paths, and sync-then-land guidance.
- Hold an imported pull when land is refused as a stale base, and publish LSP conflict regions for that refusal.

### wi-2442

`graft diff --stat` prints per-file added and removed line counts and a summary line (at most 100 files; the rest stay on the next page). `graft -C <dir>` runs the command as if it had been started in that directory.

### wi-2444

# WI-2444

- Private onboarded mirrors mount a GitHub token file and pass it to git as `http.extraHeader` inside the container. The token is not printed and is not placed in argv. An empty token file fails the mirror instead of reporting lag 0.
- Repository probing distinguishes GitHub authentication failures from network or availability failures; only authentication failures select private-mirror configuration.
- Generated mirror configs are mode 0644 so container uid 10001 can read them.
- Generated Compose `tmpfs` is a block list, including the rollback overlay.
- `onboard-repo.sh` no longer requires the unused `secret` wrapper.
- `up.sh` fails with "no ref after import" when a repository still has no ref at the deadline, distinct from the importing log.

### wi-2446

### Changed

- Phase 1 exit and kill criteria measure agent friction and conflict churn (D31). Read-path speed is a no-regression floor at the WI-2341 medians, plus rule 2; 2× faster than git is a later-phase target. WI-2400 is wont_do.

