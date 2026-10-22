# graft build plan

- **Status:** Draft 1 (2026-10-22)
- **Design:** [design.md](design.md) · **Decisions:** [decisions.md](decisions.md)

Every phase ends in something we can ship and use. Phase 1 is a **read
pilot** on a mirrored real repository (rustTorrent). Each later phase moves
one more part of the workflow onto graft, and none takes away the fallback to
git and GitHub until phase 3's authority switch, which can be rolled back.

| Phase | Ships | Fallback |
| --- | --- | --- |
| 0 | Workspace skeleton, design, plan (this commit) | — |
| 1 | **Read pilot:** rustTorrent mirrored into graft (git-primary). Agents clone sparse slices and ask log, diff, blame and read through the CLI and MCP | Git is untouched; agents can ignore graft |
| 2 | **Write through GitHub:** agents commit drafts in graft, and `graft land` goes through a GitHub PR into the existing merge queue. Node daemon on shared hosts | The GitHub workflow is unchanged; graft is a front end to it |
| 3 | **Native landing:** server rebase and land; graft becomes authoritative for rustTorrent `main`, exported to GitHub. GC, LSP, CI attestations | Authority switch back to git-primary (§11.3 of the design) |
| 4 | **Breadth:** per-line combiner, prefetch, auto-revert, symbol index, a second repository, the export drill | Per repository, as for phase 3 |
| 5 | **Later:** regional tier, virtual checkouts, chunked blobs, code search, publish decision | — |

## How the work is run

### Work item format

Each item is sized for **one worker session** and states:

- **Crates:** the only crates it may edit;
- **Needs:** the items that must have landed first;
- **Deliver:** what it builds;
- **Accept:** the acceptance criteria, which are the review checklist;
- **Test:** the test strategy.

Items are numbered `G<phase>.<n>`, and each becomes one Vogt work item and one
branch (`g<phase>-<n>-<slug>`).

### Crate ownership and parallelism

Items are grouped in **waves of three**. Within a wave, the three items own
disjoint crates, so three workers can run at once without touching the same
files. The rules that keep that true:

1. **One crate, one owner per wave.** An item edits only its listed crates.
2. **Frozen interfaces.** The skeleton fixes each crate's public types and
   trait signatures. If an item needs to change another crate's public API, it
   stops and raises a small *interface PR* against that crate. The interface
   PR lands first (it is reviewed quickly, has no logic, and may stub
   bodies), and then the dependent work goes on.
3. **Program against fakes.** When a dependency's bodies are not built yet, a
   worker tests against the in-memory store and the recording fakes in
   `graft-testkit`, not against the unfinished crate.
4. **Workspace root files.** The root `Cargo.toml`'s `[workspace.dependencies]`
   is one dependency per line, in sorted order. An item that needs a new
   dependency adds its one line. A `Cargo.lock` conflict on rebase is resolved
   by regenerating it (`cargo update -w`), never by hand-merging.
5. **Docs and changelog.** An item edits only the docs for its crate
   (`crates/<crate>/README.md`) and adds a changelog fragment
   `changes/<item>.md`. It does not edit `docs/design.md`. Design changes are a
   separate, operator-reviewed PR.
6. **`graft-testkit` is shared.** Each item adds its own module file
   (`src/<area>.rs`) and one sorted `pub mod` line in `lib.rs`.

### Worker checks

Workers run targeted checks only, with `CARGO_BUILD_JOBS=4` on shared hosts:

```sh
cargo fmt --all -- --check
cargo clippy -p <crate> --all-targets -- -D warnings
cargo nextest run -p <crate>        # or cargo test -p <crate>
```

CI runs the whole workspace. Every implement brief ends with: *stop once
pushed; don't poll CI; print AUTOPILOT: DONE.*

### Test strategy (applies to every item)

| Technique | Where |
| --- | --- |
| Golden vectors | Every durable encoding (ids, tree, changeset, frame, token). A change fails the build |
| Property tests (`proptest`) | Encode/decode round-trips. Tree apply and diff against a naive `BTreeMap` model. Rebase against a brute-force model |
| Conformance suite | Every `ObjectStore` and `MetaStore` implementation runs the same suite from `graft-store` |
| Crash tests | A testkit harness runs a writer as a child process, `kill -9`s it at random points, restarts it, and checks that every acknowledged write is present and every invariant holds |
| Differential tests against git | Import and export of synthetic repositories and rustTorrent. Log, diff and blame results compared with `git log --first-parent`, `git diff`, `git blame --first-parent` |
| Concurrency tests | Racing lands, CAS storms, single-flight in the node daemon. Deterministic schedules where possible |
| Fuzzing (`cargo-fuzz`) | Decoders for frames, changesets, trees, tokens, and the wire messages. Run in CI nightly, not on PRs |
| Benchmarks | The testkit pilot harness reproduces the OpenScion trial scenarios against git on the same host |

---

## Phase 0: skeleton (done in this commit)

- A Cargo workspace of 19 crates with frozen public APIs and `todo!()` bodies.
  It passes `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets`.
- `docs/design.md`, `docs/plan.md`, `docs/decisions.md`.

**Before phase 1 starts**, the operator answers the phase 1 blockers in
decisions.md. (All phase 1 decisions are decided: D2, D3, D4, D5, D7,
D11, D12, D13, D14, D15 and D18.)

---

## Phase 1: read pilot

**Goal:** rustTorrent's full history is in graft and follows GitHub `main`
within a minute. Agent sessions on Node B can `graft clone` sparse slices and
ask `log`, `diff`, `blame`, `show`, and `cat` through the CLI or MCP. Storage
is durable, and every acknowledged write survives `kill -9`. Writes still go
through git and GitHub.

### Wave 1.A

**G1.1 Model: ids, paths, objects, changesets.**
- Crates: `graft-model`
- Needs: —
- Deliver: BLAKE3 ids with kind separation and text forms; `RepoPath` rules
  (including portability checks); tree and changeset canonical encodings and
  decoders; `apply_changes(parent_root, changes)` by path copying, and
  `diff_trees(a, b)` pruned by id, over the `ObjectSource` trait; the
  changeset validity rules that need no store.
- Accept: golden ids for a fixed blob, tree and changeset are pinned. Hex
  parsing accepts exactly the documented forms. Each path rule and each
  validity rule has its own test. `apply_changes` on a one-file change in a
  10,000-file tree writes only the trees on that path (asserted by counting
  writes). `diff_trees` reads only differing subtrees.
- Test: golden vectors; proptest round-trips; proptest of apply and diff
  against a `BTreeMap<path, entry>` model; a fuzz target for each decoder.

**G1.2 Storage ports, memory store, conformance suite.**
- Crates: `graft-store`, `graft-testkit` (the `clock`, `synth` modules)
- Needs: —
- Deliver: the final `ObjectStore` and `MetaStore` traits; an in-memory
  implementation (behind feature `testkit`); a conformance suite that any
  implementation can run (`graft_store::conformance::run_all(factory)`); a
  testkit fake clock and a synthetic repository generator (N files, depth D,
  history H, seeded).
- Accept: the memory store passes the whole suite. The suite covers
  put-if-absent idempotency; ref CAS (stale, absent, success, version
  monotonicity); the journal written in the same transaction as each write;
  LSN monotonicity; workspace CAS and leases; holder rows; and pagination
  bounds.
- Test: the suite runs against the memory store. Suite cases are written as
  black-box tests against the traits only.

**G1.3 Protocol: API traits, wire types, errors.**
- Crates: `graft-proto`
- Needs: —
- Deliver: the request, response and error types for every phase 1
  operation; the `ObjectApi`, `RepoApi`, `WorkspaceApi` and `AdminApi`
  traits; protocol version binding; the object frame format (magic, kind,
  codec, length; gzip via `flate2`); the `.proto` definitions and generated
  gRPC code via `tonic-build` with `protox` (no `protoc` needed); and
  conversions between Rust types and protobuf.
- Accept: every type round-trips through protobuf (proptest). The version
  binding rules are tested. The frame codec chooses raw or gzip by the
  documented thresholds. The decoder rejects bad magic, unknown codecs, and a
  length mismatch. Each error converts to and from a gRPC status losslessly.
- Test: proptest round-trips; golden frames; a frame decoder fuzz target.

### Wave 1.B

**G1.4 Derived data: framework and eager kinds.**
- Crates: `graft-index`
- Needs: G1.1, G1.2
- Deliver: the derived-kind framework (kind, version, needed parents, eager
  or lazy, cache keyed by `(kind, ver, key)`); generation numbers; first-parent
  skip lists; line positions and line path index rows (including directory
  prefixes), produced as row sets for the engine to commit in its
  transactions.
- Accept: generation and skip list values match a brute-force model on
  random DAGs (including merges and several roots). The line path index rows
  for a changeset are O(changes × depth). Lazy derivation walks back only to
  the nearest computed value (asserted by counting reads).
- Test: proptest random DAGs against brute force; read and write counting
  with the memory store.

**G1.5 Filesystem object store.**
- Crates: `graft-store-fs`
- Needs: G1.2, G1.3 (frame format)
- Deliver: the `ObjectStore` on disk as designed (sharded layout, temp file,
  fsync, no-replace rename, directory fsync); stray temp cleanup at open;
  verify on read; `stat`; usable by both the server and the node (with an
  LRU-bounded mode for the node).
- Accept: passes the conformance suite. A crash test (`kill -9` during 10,000
  concurrent puts, 20 iterations) shows no torn or missing acknowledged
  object. A corrupted file yields `CorruptObject`. LRU mode never exceeds its
  byte bound by more than one object.
- Test: the conformance suite; the testkit crash harness (adds module
  `crash`); fault injection by truncating and flipping bytes.

**G1.6 SQLite metadata store.**
- Crates: `graft-store-sqlite`
- Needs: G1.2
- Deliver: the `MetaStore` on SQLite (WAL, `synchronous=FULL`, a single writer
  thread with a bounded queue, a reader pool); versioned migrations; the
  schema in design §6.5; backpressure as `Unavailable{retry_after}` when the
  queue is full.
- Accept: passes the conformance suite. A crash test (`kill -9` during mixed
  ref CAS, changeset inserts and workspace updates, 20 iterations) shows every
  acknowledged write present and journal LSNs gap-free and topological.
  Migrations from an empty database and from each prior version are tested.
  1,000 ref CAS operations per second sustained on Node B.
- Test: the conformance suite; the crash harness; a migration test; a
  throughput micro-benchmark.

### Wave 1.C

**G1.7 Engine: write path and snapshot reads.**
- Crates: `graft-engine`
- Needs: G1.1, G1.2, G1.3, G1.4
- Deliver: `RepoApi` and `AdminApi` (phase 1 subset) over generic ports:
  `CreateRepo`, `PutObjects`, `HasObjects`, `GetObjects` (partial results),
  `CreateChangeset` (all validity rules, root derivation, eager kinds, the
  journal), `ImportChangesets` (bulk, roots allowed), `GetChangesets`,
  `ResolveRef`, `ListRefs`, admin `UpdateRef` (line fast-forward enforced),
  `Stat`, `ListTree` (paginated), `ReadFile`, `Diff` (names and patches,
  capped), and `Watch` over the journal.
- Accept: the phase 1 API round-trip (upload, create, ref, resolve, fetch)
  reproduces every file of a synthetic repository. Each error that phase 1
  operations can return has a test that produces it. `GetObjects` with some
  ids missing returns the rest. A `CreateChangeset` retry returns the same id
  and writes nothing new. A non-fast-forward line move is refused.
- Test: in-process tests against the memory store and the SQLite store;
  proptest of `Diff` against brute-force snapshot comparison.

**G1.8 Git import.**
- Crates: `graft-git`
- Needs: G1.1, G1.3
- Deliver: reading a bare git repository with `gix`; a topological walk of
  every commit reachable from the configured refs; first-parent tree diffs
  walking only changed subtrees; mode mapping; blob upload and
  `ImportChangesets` batches through the `RepoApi` trait; mirror DB records
  (the git sha, raw headers, raw non-canonical trees); the export functions
  needed to *verify* fidelity (sha recomputation); a path rule report.
- Accept: on synthetic git repositories with merges, several roots,
  symlinks, executables, submodules, non-canonical modes, empty merges, signed
  commits and non-UTF-8 messages, every commit satisfies
  `export(import(c)).sha == c.sha`. Bad paths stop the import with a full list.
  Import of rustTorrent completes with zero exceptions.
- Test: synthetic repositories built with `git` in temp dirs; a recording
  `RepoApi` fake from the testkit (module `fake_api`); a differential check of
  every imported snapshot against `git ls-tree -r`.

**G1.9 Auth and telemetry.**
- Crates: `graft-auth`, `graft-telemetry`
- Needs: —
- Deliver: the token format (Ed25519, PASETO v4.public or the chosen
  equivalent), mint and verify, scopes and path rules, and a `jti` denylist
  interface; the telemetry init (JSON tracing, the Prometheus registry, OTLP
  behind a feature) and the metric name registry from design §13.
- Accept: golden tokens. Expiry, wrong key, scope and path-prefix checks
  each have a test. A token value never appears in a `Debug` or `Display`
  string, enforced by a test. Every metric in design §13 is registered under
  its documented name and labels.
- Test: unit tests and golden vectors; a log-capture test that redaction
  holds.

### Wave 1.D

**G1.10 History queries: Log, ancestry, Blame.**
- Crates: `graft-index`
- Needs: G1.4, G1.7 (for integration tests; unit tests use the memory store)
- Deliver: `Log` (line range scans, draft walk to the line, path and
  directory filters, pagination), `IsAncestor` and `MergeBase` (generation and
  line-position pruning), and `Blame` as a lazy, cached kind (histogram diff,
  for example via `imara-diff`).
- Accept: on rustTorrent, `Log --path` for each of 50 sampled paths equals
  `git log --first-parent --format=%H -- <path>` (through the git sha map).
  `Blame` of 20 sampled files equals `git blame --first-parent` line origins.
  `MergeBase` and `IsAncestor` match brute force on random DAGs. A path `Log`
  reads O(results) index rows (asserted).
- Test: differential tests against git on rustTorrent and synthetic
  repositories; proptest against brute force.

**G1.11 Client library: fetch, working copy, sparse, status.**
- Crates: `graft-client`
- Needs: G1.3, G1.5
- Deliver: the remote `ObjectApi` and `RepoApi` over gRPC (TCP and Unix
  socket, gzip, deadlines); endpoint discovery (environment variable, node
  socket, config); pipelined breadth-first fetch; a local frame cache (using
  `graft-store-fs` in LRU mode); the working copy layout (`.graft/`);
  multi-prefix sparse; stat-based status with racily-clean handling; and
  update from base A to B by `Diff`.
- Accept: a clone with three prefixes fetches only objects under them
  (asserted by counting requests). An interrupted fetch resumes by asking only
  for the missing ids. 50 back-to-back same-size edits within one second are
  all detected by status. Update from A to B fetches only the blobs `Diff`
  names.
- Test: against an in-process engine server on a loopback socket (testkit
  module `server`); fault injection by dropping the connection mid-stream.

**G1.12 Server binary.**
- Crates: `graft-server`
- Needs: G1.3, G1.7, G1.9
- Deliver: the gRPC services wired to the engine over the SQLite and
  filesystem stores; TOML config; auth middleware; `/metrics`, `/healthz`,
  `/readyz`; the read-only JSON facade; admin subcommands (`token mint`,
  `fsck`, `backup`, `repo create`); a free-space admission floor; and graceful
  shutdown.
- Accept: an end-to-end test (start the binary, import a synthetic
  repository through the client, `kill -9`, restart, fetch everything, `fsck`
  is clean). An unauthenticated or out-of-scope call is refused with the
  right error. Restart to serving takes under 2 s with 1 GB of objects.
- Test: process-level integration tests through the testkit `server` and
  `crash` modules.

### Wave 1.E

**G1.13 CLI: read commands.**
- Crates: `graft-cli`
- Needs: G1.11
- Deliver: `clone`, `status`, `diff`, `log`, `show`, `blame`, `cat`, `ls`,
  `sparse`, `api`, and `version`, each with `--json` (a stable schema, written
  as JSON Schema under `crates/graft-cli/schemas/`), the exit codes from design
  §8.2, and no prompts without a TTY.
- Accept: a snapshot test of the human and JSON output of every command. The
  JSON schemas validate. The parity test exists and lists every phase 1 API
  operation as having a command or an explicit exclusion.
- Test: `assert_cmd` against the testkit server; snapshot tests (`insta`).

**G1.14 Mirror: import and follow.**
- Crates: `graft-mirror`
- Needs: G1.8, G1.11
- Deliver: `graft-mirror import` and `graft-mirror follow` (git-primary):
  `git fetch` into a local bare mirror; poll `ls-remote` every 15 s; import
  new commits; move graft lines by admin CAS; the mirror DB (SQLite); and the
  lag metrics and divergence detection (GitHub moved non-fast-forward, which
  stops and alerts).
- Accept: against a local "GitHub" (a bare repository), new commits appear in
  graft within one poll. A force-push on the source raises divergence and
  changes nothing in graft. A restart resumes from the mirror DB with no
  duplicate imports.
- Test: integration tests with local bare repositories standing in for
  GitHub.

**G1.15 MCP: read tools.**
- Crates: `graft-mcp`
- Needs: G1.3
- Deliver: an MCP server over the API traits (stdio and streamable HTTP
  transports as library functions): `graft_log`, `graft_show`, `graft_diff`,
  `graft_blame`, `graft_read`, `graft_ls`, `graft_status`, with bounded,
  paginated results, `readOnlyHint`, and resources.
- Accept: each tool is tested against a fake API with golden JSON outputs.
  No tool result exceeds its documented size bound. Tool schemas are
  validated with the MCP inspector or an equivalent conformance check.
- Test: unit tests over the testkit `fake_api`; one end-to-end test through
  stdio.

### Wave 1.F

**G1.16 Pilot harness and crash suite.**
- Crates: `graft-testkit` (modules `pilot`, `bench`), `tests/` at the
  workspace root
- Needs: G1.12, G1.13, G1.14
- Deliver: a harness that imports a git mirror, verifies every changeset
  against `git checkout` (paths, modes, symlinks, bytes), and runs the
  trial's scenarios against git on the same host: full and sparse clone,
  log, diff, blame, and decompression share of checkout time (for D5). It
  writes `results.tsv` plus a Markdown report. It also runs the long crash
  suite (100 `kill -9` restarts under mixed load).
- Accept: it runs end to end on a scratch rustTorrent mirror on a
  workstation. Results are reproducible within 10% across three runs.
- Test: the harness itself runs in CI on a small synthetic repository.

**G1.17 Pilot deployment (ops repo).**
- Crates: none in this repository (Forgejo `indexarr/ops`, Komodo)
- Needs: G1.12, G1.14, D13
- Deliver: a Komodo stack for `graft-server` and `graft-mirror` on Node B
  (volumes for objects, SQLite and the mirror DB; tailnet bind); Litestream or
  the chosen backup; a Prometheus scrape; the alerts from design §13; a
  runbook (start, stop, restore, re-import, mint a token); and the read token
  in Infisical.
- Accept: a deploy through Komodo with a healthy health check. Restore
  rehearsed from backup onto a scratch path, with `fsck` clean. Alerts fire in
  a test (lag simulated by stopping follow).
- Test: the rehearsal is recorded in the runbook.

**G1.18 Pilot run and verdict.**
- Crates: none (`docs/trials/`)
- Needs: G1.16, G1.17
- Deliver: rustTorrent imported and followed for 7 days. Agent sessions in
  the rustTorrent lane are told to use `graft clone` and `graft log/diff/blame`
  (or MCP) for reads. A trial report in the house style of the OpenScion
  trial, with the exit criteria scored.
- Accept: the [exit criteria](#phase-1-exit-and-kill-criteria) are scored with
  evidence, and the operator gives a go or no-go.

### Phase 1 exit and kill criteria

**Superseded by [D20](decisions.md#d20-no-pilot-period):** there is no
pilot period, and G1.18 is dropped. These criteria are targets that the G1.16
harness reports on. They don't gate phase 2.

**Exit (all must hold):**

1. Fidelity: 100% of rustTorrent commits pass the import fidelity invariant,
   and every snapshot matches `git checkout`.
2. Durability: zero acknowledged writes lost across 100 `kill -9` restarts.
   Restore from backup is rehearsed.
3. Follow lag: p95 under 60 s over 7 days. No undetected divergence.
4. Correctness: `log`, `diff` and `blame` agree with git on the sampled sets.
5. Performance, on Node B: sparse clone p50 at most 50 ms, `log --path` p50 at
   most 10 ms, `blame` cached p50 at most 10 ms, and full clone p50 at most
   150 ms.

**Kill or rethink (any one):**

- Agents' read paths on graft are not at least 2× faster than the git
  equivalents they replace, end to end.
- Agents do not use it unprompted after a week, according to the session
  logs.
- Operating it costs more than half a day a week of operator or oversight
  time.

---

## Phase 2: write through GitHub

**Goal:** agents commit drafts in graft and land them through GitHub. `graft
land` exports the stack as a PR into the existing merge queue, and the merged
commit returns through follow and marks the drafts landed. A node daemon runs
on Node B. Sessions hold leased workspaces tied to their Vogt session. Review,
CI and the merge queue do not change.

### Wave 2.A

**G2.1 Engine: workspaces, drafts, holders.**
- Crates: `graft-engine`
- Needs: G1.7
- Deliver: the full `WorkspaceApi` (`OpenWorkspace`, `RenewLease`,
  `SetHead` by CAS, `ListWorkspaces`, `CloseWorkspace`); `CreateChangeset`
  with `workspace` (head CAS in the same transaction); draft records and
  states; object holder rows and upload leases; `graft_orphans{state}` and
  `graft_workspaces{state}` gauges.
- Accept: a draft never exists without a holder (a property test over random
  operation sequences). An expired lease moves drafts to abandoned. `SetHead`
  with a stale expectation fails and changes nothing. Orphan gauges match a
  brute-force count.
- Test: model-based property tests (random op sequences against a reference
  model); conformance with both stores.

**G2.2 Git export.**
- Crates: `graft-git`
- Needs: G1.8
- Deliver: deterministic export of native changesets (path-copying git trees,
  the git blob sha cache, committer and trailer rules), `export --to <bare>`
  for a full repository, and the fidelity check as a reusable function.
- Accept: exporting twice gives byte-identical commits. The export of a
  repository built natively in graft passes `git fsck --strict`, and its
  checkouts equal graft's snapshots. An imported-then-exported rustTorrent
  equals the source repository ref for ref.
- Test: differential against `git` (`fsck`, `ls-tree`, `cat-file`); a
  property test that export after import is the identity on synthetic
  repositories.

**G2.3 Node daemon.**
- Crates: `graft-node`
- Needs: G1.3, G1.5, G1.9
- Deliver: `graft-node` serving `ObjectApi` from an LRU disk cache over a Unix
  socket; single-flight; forwarding repository RPCs with the caller's token;
  local token verification; bounded upstream concurrency and load shedding;
  metrics; a systemd unit.
- Accept: 30 concurrent clones of the same slice cause exactly one upstream
  fetch per object. With the upstream down, cached reads still work and
  forwarded calls get `Unavailable` quickly. A corrupted cache entry is
  detected, discarded and refetched. The node never caches a mutable answer
  (a test asserts every repository RPC is forwarded).
- Test: concurrency tests with a counting fake upstream; fault injection.

### Wave 2.B

**G2.4 Server: workspaces, Watch, tokens.**
- Crates: `graft-server`, `graft-auth`
- Needs: G2.1
- Deliver: wire the workspace RPCs and the streaming `Watch`; `MintToken` for
  a Vogt service principal (scoped and with a TTL); the `jti` denylist store;
  node token passthrough; audit fields (`session`, `work_item`) on every
  span.
- Accept: a Vogt-scoped token mints a session token no broader than its own
  scope (tested). `Watch` resumes from an LSN with no gaps or duplicates after
  a server restart. A revoked `jti` is refused on writes.
- Test: process-level integration tests through the testkit server.

**G2.5 Client and CLI: write commands.**
- Crates: `graft-client`, `graft-cli`
- Needs: G2.1 (via the API traits), G1.13
- Deliver: `commit [--amend] [-m]`, `workspace`, `stack list`, and `sync` (by
  server rebase once G2.7 lands; until then, sync only fast-forwards a base
  with no stack). Lease heartbeats in long-running commands. `land` in PR
  bridge mode (`Queued { pr_url }`, `--wait`).
- Accept: commit uploads only blobs the server lacks (`HasObjects`).
  Committing in a sparse slice never touches paths outside it. `--amend`
  supersedes the old head. The parity test is updated.
- Test: `assert_cmd` against the testkit server; snapshot tests.

**G2.6 Mirror: PR bridge.**
- Crates: `graft-mirror`
- Needs: G2.2, G1.14
- Deliver: a land request on a git-primary line becomes an exported branch
  `graft/<workspace>/<id>`, a PR (with the work item and session in the body)
  and a merge-queue enqueue. It tracks the PR and checks. When the
  `Graft-Changeset` trailer comes back through follow, the drafts are marked
  landed. A closed or failed PR is reported as `Rejected` with a URL. It uses
  a GitHub App (D8).
- Accept: against a local bare repository plus a fake GitHub API, the full
  lifecycle works: land, PR, merge (squash or rebase), follow, landed. A
  closed PR gives `Rejected`. A restart mid-lifecycle resumes correctly.
- Test: integration tests with a fake GitHub HTTP server (`wiremock`).

### Wave 2.C

**G2.7 Engine: file-level rebase and Sync.**
- Crates: `graft-engine`
- Needs: G2.1
- Deliver: `Rebase` (the file-level table in design §7.8, including
  file/directory clashes, deterministic ids, stack bottom-up), and
  `Sync(workspace)`.
- Accept: rebase matches a brute-force model on random histories. Identical
  changes on both sides are dropped. A conflict creates nothing. Repeating
  the rebase returns the same id. Cost is independent of the distance to
  `onto` (asserted by counting reads).
- Test: proptest against a model; read and write counting.

**G2.8 MCP: write tools; server `/mcp`.**
- Crates: `graft-mcp`
- Needs: G1.15, G2.1 (via the API traits)
- Deliver: `graft_commit` (inline content, or from the working copy),
  `graft_sync`, `graft_land` (PR bridge mode), `graft_workspace`; the
  streamable HTTP transport ready to mount in the server.
- Accept: a checkout-less MCP agent can read, commit a two-file change with
  inline content, and land it through the PR bridge, in an end-to-end test
  against the fakes.
- Test: an end-to-end test over the testkit server plus a fake mirror.

**G2.9 GC reporting and the Vogt integration.**
- Crates: `graft-gc`; the Vogt repository (a separate PR there)
- Needs: G2.1, G2.4
- Deliver: `Gc { dry_run: true }`, which reports what would be collected (no
  deletes yet). On the Vogt side: mint a graft token at session start, renew
  the workspace lease while the session lives, close it when the session
  stops, and link landed changesets to work items through the journal.
- Accept: the dry-run report matches a brute-force computation. A Vogt
  session that stops leaves its workspace closed within one minute.
- Test: model tests; a Vogt-side integration test against a testkit server.

### Phase 2 exit

The rustTorrent lane lands through graft for 2 weeks with **no** increase in
landing failures compared with the git baseline. Every landed PR is linked to
its drafts. No orphan is left unaccounted for: every draft is landed,
superseded, or held, or else it is reported abandoned.

---

## Phase 3: native landing (graft-primary for rustTorrent)

**Goal:** graft `main` is authoritative for rustTorrent. Lands are native
(server rebase and CAS, with a policy gate). GitHub `main` is an exported
mirror, written only by the mirror's App. Orphans are collected. The LSP
server ships.

### Wave 3.A

**G3.1 Engine: Land, policies, Attest.**
- Crates: `graft-engine`
- Needs: G2.7
- Deliver: `Land` exactly as in design §8.1 (idempotency record, attempt
  budget, backoff, land-attempt leases, line index rows in the CAS
  transaction); the `direct`, `attested` and `queued` policy hooks; `Attest`;
  path-rule requirements.
- Accept: 8 concurrent lands on disjoint files all land, and the line ends
  with all of them. Concurrent lands on the same lines of one file give exactly
  one `Conflict` (file-level). A retried land returns the same `Landed`. A
  line never moves backwards (property test under racing lands). Uncontended
  land p50 is at most 15 ms on Node B.
- Test: concurrency tests with real threads against the SQLite store; a
  model-based property test of racing lands; a benchmark.

**G3.2 GC: sweep.**
- Crates: `graft-gc`
- Needs: G2.9
- Deliver: mark, doom, grace, delete and revive as in design §8.6; batched
  and rate-limited; journalled; metrics.
- Accept: a property test over random interleavings of create, upload,
  land, close and GC never deletes a held or landed object. Revival during
  the grace period works. A delete followed by a reference yields
  `MissingObjects` and a successful re-upload. Reclaimed bytes match the
  model.
- Test: model-based concurrency tests; the crash harness during a sweep.

**G3.3 Mirror: export mode.**
- Crates: `graft-mirror`
- Needs: G2.2, G2.4
- Deliver: graft-primary export (tail `Watch`, export each line move, push
  with lease, persist progress); divergence stop and alert; `reconcile`;
  importing external GitHub PRs as drafts in `github-pr/<n>` workspaces.
- Accept: 1,000 random lands are exported in order with deterministic shas. A
  `kill -9` at any point resumes with no duplicate and no gap. An out-of-band
  push stops export and alerts, and never forces.
- Test: integration tests with a local bare repository and a fake GitHub;
  the crash harness.

### Wave 3.B

**G3.4 Model: three-way text merge.**
- Crates: `graft-model`
- Needs: G1.1
- Deliver: a pure `merge3(base, ours, theirs) -> Clean(bytes) | Conflicts(regions)`
  over the histogram diff, with text detection (UTF-8, no NUL, under 1 MiB),
  plus conflict-marker rendering for clients.
- Accept: it agrees with `git merge-file` on a corpus of 500 generated cases
  (cleanliness and merged bytes), allowing documented differences. Merging is
  symmetric (ours and theirs swapped gives the mirror result).
- Test: a differential corpus against `git merge-file`; proptest invariants
  (merging with itself is the identity).

**G3.5 Client and CLI: native land, conflicts, revert, stack.**
- Crates: `graft-client`, `graft-cli`
- Needs: G3.1 (via the API traits)
- Deliver: `land` (native) with the result table in design §8.2; conflict
  materialization (markers and sidecars); `revert`; `stack
  squash|reorder|split|drop`; `attest` for CI.
- Accept: every land result maps to its exit code and output (snapshot
  tests). After `Conflict`, `sync` writes markers, and an edit, commit and land
  succeeds. The working copy is never left on an unreachable base (a property
  test over random race outcomes).
- Test: `assert_cmd` against the testkit server with injected races.

**G3.6 LSP server.**
- Crates: `graft-lsp`, plus the `graft-cli` `lsp` subcommand wiring (one
  file: `src/cmd/lsp.rs`)
- Needs: G1.11, G1.10 (via the API traits)
- Deliver: hover blame, code lenses, diagnostics (conflicts, upstream
  changes, edits outside the slice), commands (commit, land, sync, diff
  base), and `graft:` virtual documents.
- Accept: an LSP conformance test with a scripted client (initialize, hover,
  codeLens, executeCommand) passes. It works with VS Code and Helix as the
  manual smoke test, recorded with screenshots in the PR.
- Test: a scripted LSP client (`tower-lsp` test utilities or an equivalent).

### Wave 3.C

**G3.7 Engine: Revert, stack ops, line merge.**
- Crates: `graft-engine`
- Needs: G3.1, G3.4
- Deliver: `Revert`; `Stack` operations; `merge = line` in `Rebase`, `Sync`
  and `Land` (D16), with `graft.merged-paths`; and `attest_merged`.
- Accept: revert of revert restores the original tree. Two lands editing
  different regions of one file both land and the result equals `merge3`.
  `attest_merged` forces re-attesting.
- Test: proptest; concurrency tests reused from G3.1.

**G3.8 CI integration.**
- Crates: `graft-auth` (OIDC exchange), workflow templates in `ci/`
- Needs: G3.1, G3.3
- Deliver: exchange GitHub Actions OIDC (and Woodpecker job) tokens for short
  graft tokens; a reusable workflow that runs required checks on
  `graft/*` draft branches and posts `Attest`; post-land CI on the mirror's
  pushes.
- Accept: a draft gains an attestation from a real (or `act`-run) workflow,
  and an `attested` land succeeds only after it. An OIDC token for another repo
  is refused.
- Test: unit tests with recorded JWKS and tokens; one live rehearsal.

**G3.9 Authority switch tooling and rehearsal.**
- Crates: `graft-mirror`
- Needs: G3.3
- Deliver: `graft-mirror authority {status,freeze,verify,flip,unfreeze}`
  implementing design §11.3 in both directions, and a runbook.
- Accept: a full A → B → A rehearsal on a scratch copy of rustTorrent, with
  a scratch GitHub repository, loses nothing and ends sha-identical.
- Test: the rehearsal is scripted and recorded.

### Wave 3.D

**G3.10 Cutover of rustTorrent.**
- Crates: none (operations)
- Needs: G3.1–G3.9, D6, D8
- Deliver: rustTorrent `main` flipped to graft-primary in a scheduled window;
  the merge-queue rule replaced by the line policy; delivery-process docs
  updated for the repository.
- Accept: 2 weeks with no divergence, GitHub always within 60 s of graft,
  land p50 under 1 s including policy checks, and rollback still possible (the
  rehearsal is re-run on a scratch copy at the end).

---

## Phase 4: breadth (outline)

Each becomes items in waves of three when phase 3 exits.

| Item | Crates | Accept (summary) |
| --- | --- | --- |
| Per-line land combiner | `graft-engine` | 64 racing lands: total time at most 2× the single-land time; same results as without the combiner |
| Node prefetch via `Watch` | `graft-node` | A fresh clone of the tip hits the node cache at 99% or more |
| Auto-revert bot | `graft-mirror` or a new `graft-bots` crate | A failing post-land check produces a revert draft that lands within its SLA |
| Symbol index (derived `symbols`) for LSP and MCP | `graft-index`, `graft-lsp`, `graft-mcp` | Workspace symbols across the whole repository from a sparse checkout |
| Filesystem watcher for status | `graft-client` | Status in O(changed files) on a 100,000-file synthetic tree |
| Postgres `MetaStore` (if D3 says so) | `graft-store-postgres` | Passes conformance and the crash suite; two server replicas land concurrently |
| Second repository (for example vogt) | operations | Phase 1 to 3 criteria, compressed |
| Weekly export drill | `graft-mirror`, ops | An automated full export compared with GitHub, with a report |
| Path read restrictions | `graft-engine`, `graft-auth` | Restricted paths are absent from path-addressed reads for a principal without the scope |

## Phase 5: later

Regional cache tier; virtual (FUSE) checkouts; chunked large blobs with
verified streaming; content search index; copy and rename detection; a zstd
re-encode with a dictionary if D5's measurement calls for it; the name and
publish decisions (D1, D2).

## Item index

| Item | Wave | Crates |
| --- | --- | --- |
| G1.1 | 1.A | graft-model |
| G1.2 | 1.A | graft-store, graft-testkit |
| G1.3 | 1.A | graft-proto |
| G1.4 | 1.B | graft-index |
| G1.5 | 1.B | graft-store-fs |
| G1.6 | 1.B | graft-store-sqlite |
| G1.7 | 1.C | graft-engine |
| G1.8 | 1.C | graft-git |
| G1.9 | 1.C | graft-auth, graft-telemetry |
| G1.10 | 1.D | graft-index |
| G1.11 | 1.D | graft-client |
| G1.12 | 1.D | graft-server |
| G1.13 | 1.E | graft-cli |
| G1.14 | 1.E | graft-mirror |
| G1.15 | 1.E | graft-mcp |
| G1.16 | 1.F | graft-testkit, tests/ |
| G1.17 | 1.F | ops repo |
| G1.18 | 1.F | docs/trials |
| G2.1 | 2.A | graft-engine |
| G2.2 | 2.A | graft-git |
| G2.3 | 2.A | graft-node |
| G2.4 | 2.B | graft-server, graft-auth |
| G2.5 | 2.B | graft-client, graft-cli |
| G2.6 | 2.B | graft-mirror |
| G2.7 | 2.C | graft-engine |
| G2.8 | 2.C | graft-mcp |
| G2.9 | 2.C | graft-gc, vogt |
| G3.1 | 3.A | graft-engine |
| G3.2 | 3.A | graft-gc |
| G3.3 | 3.A | graft-mirror |
| G3.4 | 3.B | graft-model |
| G3.5 | 3.B | graft-client, graft-cli |
| G3.6 | 3.B | graft-lsp, graft-cli (one file) |
| G3.7 | 3.C | graft-engine |
| G3.8 | 3.C | graft-auth, ci/ |
| G3.9 | 3.C | graft-mirror |
| G3.10 | 3.D | operations |

Note: G3.6 touches one file in `graft-cli` (`src/cmd/lsp.rs`) while G3.5 owns
`graft-cli`. The skeleton already has that file as a stub, and G3.5 must not
edit it, so the two do not collide.
