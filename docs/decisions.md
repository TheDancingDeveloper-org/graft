# graft: open decisions for the operator

Each decision lists its options, the facts that matter, and a recommendation
(marked **Recommended**). The design in [design.md](design.md) is written to
the recommendations. Where a decision changes the design, the section it
touches is named.

Already decided by the operator (2026-10-22):

- **Language:** Rust.
- **Shape:** server/client, with a full API. MCP and LSP are first-class
  surfaces.
- **Git:** command names follow git where the meaning is the same, as a
  starting point, not a target. Git concepts stay out of the core model.
- **Transport compression:** gzip. See [D5](#d5-at-rest-codec) for the
  trade-off and the at-rest question that follows from it.
- **Not built on OpenScion.** It is used as a reference for lessons only, and
  no code is copied.
- **Visibility and licence (D2):** public, `AGPL-3.0-only`.
- **Source home (D15):** GitHub, `TheDancingDeveloper-org/graft` (public).
- **D3, D4, D5, D7** (operator, 2026-10-22): SQLite + filesystem; gRPC plus a
  read-only JSON facade; the gzip frame at rest; BLAKE3. Each was the
  recommendation below.
- **D11, D12, D13, D14** (operator, 2026-10-22): reject and report bad
  paths on import; a separate object namespace per repository; a Komodo stack
  on Node B; rustTorrent as the pilot. Each was the recommendation below.
- **D8, D10** (operator, 2026-10-22): a GitHub App on the org for the mirror;
  Vogt mints a graft token per session. Each was the recommendation below.
- **D21** (operator, 2026-10-23): batch PR export. Agents iterate on a
  graft integration line, and each batch reaches GitHub as one squashed PR
  that the operator merges.
- **D6, D16** (operator, 2026-10-23): `attested` landing on `main`, with
  post-land CI and auto-revert on failure; line-level merge in rebase for text
  files under 1 MiB. Each section below names the rejected options.

| # | Decision | Recommendation | Blocks |
| --- | --- | --- | --- |
| D1 | Name | Keep `graft` while private; decide before any publish | Phase 4 (publish), not before |
| D2 | Licence and visibility | **Decided:** public, AGPL-3.0-only | — |
| D3 | Storage backend | **Decided:** SQLite + filesystem through phase 3, Litestream for off-box WAL | — |
| D4 | Wire protocol | **Decided:** gRPC (tonic, prost) plus a read-only JSON facade | — |
| D5 | At-rest codec | **Decided:** store the gzip wire frame (zero transcode); benchmark zstd in the pilot | — |
| D6 | Landing gate on `main` | **Decided:** `attested` (pre-land CI on the draft) plus post-land CI with auto-revert on failure | — |
| D7 | Hash function | **Decided:** BLAKE3 | — |
| D8 | GitHub mirror identity | **Decided:** a GitHub App on the org, with ruleset bypass on `main`; key in Infisical | — |
| D9 | GitHub change notification | Poll every 15 s in phase 1; add webhooks in phase 2 if lag matters | Phase 1 (G1.12) |
| D10 | Agent token issuance | **Decided:** Vogt mints a session-scoped graft token, renews the lease, closes the workspace at stop | — |
| D11 | Path rules on import | **Decided:** reject non-UTF-8 and non-portable paths, with a report | — |
| D12 | Repository namespaces | **Decided:** one server, many repos, a separate object namespace per repo | — |
| D13 | Deployment | **Decided:** a Komodo stack on Node B; the node daemon as a systemd unit per host | — |
| D14 | Pilot repository | **Decided:** rustTorrent | — |
| D15 | Where graft's source lives | **Decided:** GitHub `TheDancingDeveloper-org/graft`, public | — |
| D16 | Line-level merge in rebase | **Decided:** on for text files under 1 MiB, recorded in metadata; larger or binary files conflict at file level | — |
| D17 | MCP hosting | Both: `graft mcp` (stdio) and the server's `/mcp` | Phase 1 (read tools), Phase 2 (write tools) |
| D18 | Phase 1 kill criteria | **Decided:** as in plan.md, unchanged | — |
| D19 | Import identity | **Decided:** `mirror.git-commit = <sha>` metadata on every imported changeset | — |
| D20 | Pilot period | **Decided:** none; G1.18 dropped, phase 2 not gated (replaces D18's gate) | — |
| D21 | Batch PR export | **Decided:** agents land on a graft integration line; each batch goes to GitHub as one squashed PR the operator merges | — |

---

## D1. Name

`graft` is a good fit (in horticulture a scion is *grafted* onto a rootstock).
But the name is crowded. There is an existing open-source Rust project called
Graft (a transactional storage and replication engine), and the name is taken
on crates.io.

- **Keep `graft`** (**Recommended while private**). It is only an internal
  name, and the crates are `publish = false`. It costs nothing until we
  publish.
- **`cambium`.** The growth layer where scion and stock join. It is
  distinctive, but there is a networking company of that name (a different
  field).
- **`rootstock`.** It pairs with "scion", but it collides with a known
  blockchain project.
- **Something else** the operator prefers.

Recommendation: keep `graft` for now. The repository is public, but no
crate is published to crates.io (`publish = false`), so the crates.io
collision does not bite yet. Rename in one PR (crate and binary names) before
the first crates.io release. The binary and crate
prefixes are kept consistent to make that a mechanical change.

## D2. Licence and visibility

**Decided (operator, 2026-10-22): public repository, AGPL-3.0.** The SPDX id
is `AGPL-3.0-only`. Switching to `AGPL-3.0-or-later` is a one-line change,
as long as it happens before any outside contributions arrive. The analysis
below is kept for the record.

- **Private repository, no licence file** (**Recommended for now**). The design
  is unproven. Phase 1 has kill criteria, and Scion's authors chose not to
  publish a generic version because it might serve no one well. OpenScion
  already exists publicly as that generic version.
- **Public, Apache-2.0 OR MIT.** The Rust ecosystem default. Best for
  adoption. Lets anyone run it as a service.
- **Public, AGPL-3.0.** Stops closed hosted forks. It deters company
  adopters, and it complicates mixing it with permissively licensed estate
  code.
- **Public, GPL-3.0** (OpenScion's choice). Copyleft for distributed binaries
  only. This is moot for a server, so it is weaker than AGPL for its own
  purpose.

Recommendation: stay private until the phase 1 verdict. If we then publish,
use Apache-2.0 OR MIT. The workspace has no `license` field yet, and every
crate is `publish = false`, so nothing can be published by accident.

## D3. Storage backend

**Decided (operator, 2026-10-22): the recommendation below.**

| Option | For | Against |
| --- | --- | --- |
| **SQLite (WAL) + filesystem objects** (**Recommended**) | No extra service, very fast on NVMe, simple backups, and it fits Node B. Durable from day one | Single writer per server. A single server process |
| Postgres + filesystem or S3 | Server replicas, standard operations | One more stateful service. Network round trips on every write. Not needed at our scale |
| FoundationDB or TiKV | Scales out, with Spanner-like semantics | Heavy to run. Overkill for us |
| Embedded KV (redb, fjall, RocksDB) | Very fast and embedded | We would hand-build the indexes and migrations that SQLite gives us. Recovery tooling is weaker |

Recommendation: SQLite + filesystem through phase 3. Ship the metadata WAL
off-box with Litestream (to MinIO, or whatever off-box target the estate uses)
plus an hourly snapshot. The `MetaStore` port is narrow enough that Postgres
can be added in phase 4 if we ever need replicas. Sub-question: is there an
estate S3-compatible store we should target for backups? Cadastre should
answer that before G1.15.

## D4. Wire protocol

**Decided (operator, 2026-10-22): the recommendation below.**

| Option | For | Against |
| --- | --- | --- |
| **gRPC (tonic + prost)** (**Recommended**) | Streaming (`GetObjects`, `Watch`), deadlines, built-in gzip, Unix sockets, code generation for other languages (Vogt is Python) | Needs codegen. It can use pure-Rust `protox`, so `protoc` is not required on every builder. Harder to `curl` |
| HTTP/1.1 + JSON (OpenScion) | Trivial to debug and call | Base64 blobs, no streaming. The trial showed per-changeset round trips |
| Connect protocol (HTTP + protobuf or JSON) | gRPC-compatible, and curl-friendly in JSON mode | A smaller Rust ecosystem |
| Custom framed binary | Fastest, fewest dependencies | Everything is hand-built: deadlines, auth, tooling, clients |

Recommendation: gRPC for every tier (TCP and Unix socket), plus a small
**read-only JSON facade** (`/v1/log`, `/v1/diff`, `/v1/read`, `/v1/blame`) on
the server for scripts and dashboards. MCP has its own transport (stdio or
streamable HTTP) and is not affected.

## D5. At-rest codec

**Decided (operator, 2026-10-22): the recommendation below.**

Transport is gzip (decided). The question is whether storage uses the same
codec.

**The trade-off (gzip compared with zstd):**

| | gzip | zstd |
| --- | --- | --- |
| Interop | Universal: gRPC built-in, HTTP `Content-Encoding`, proxies, curl | Good, but less universal |
| Compress speed | ~30–60 MB/s per core (level 6) | ~300–500 MB/s per core (level 3) |
| Decompress speed | ~300–400 MB/s | ~1–1.5 GB/s |
| Ratio on source | 5–15% larger | Better, and much better on small files with a trained dictionary |

In graft an object is compressed once (by the uploader or the server), then
stored and forwarded unchanged, so compression speed affects writes only.
Decompression happens on every checkout. At rustTorrent scale, gzip costs
about 50–70 ms of single-core decompression for a full checkout, against about
15–20 ms for zstd. That is spread across cores and smaller than the I/O.

- **Store the gzip wire frame** (**Recommended**). One format at rest and on
  the wire. The server, node, and client stream bytes with no transcode
  (Scion's "objects in the format the daemon consumes").
- **zstd at rest, transcode to gzip on serve.** Smaller disks, at the price of
  a recompression on every cache miss at every tier. This defeats the point.
- **Negotiate per connection, store both.** Twice the storage. Not worth it at
  our scale.

Recommendation: store the gzip frame. Every frame has a codec byte and ids
are over the uncompressed bytes, so moving to zstd later (with a per-repo
dictionary) is a background re-encode with no id change. The pilot harness
measures decompression share of checkout time, and we revisit if it exceeds
15%.

## D6. Landing gate on `main`

**Decided (operator, 2026-10-23): `attested` on `main`, with post-land CI and
an auto-revert on failure.**

Today every PR goes through a GitHub merge queue with required checks. Scion
drops the global queue and relies on post-merge testing.

| Option | Land latency | Safety | Fit |
| --- | --- | --- | --- |
| `direct` | ms | Post-land CI plus an auto-revert bot | Scion's model. A big change for the estate |
| **`attested`** (**Decided**) | ms after CI on the draft | Required checks pass on the draft's own tree. File-level rebases onto a newer tip are trusted. Line-merged files can require re-attesting | Keeps "required checks before land" without serializing |
| `queued` | Minutes (batched) | Tests the rebased candidate, like today | Use for release lines only |

- **Chosen:** `attested` on `main`. A land needs the required checks to pass
  on the draft's own tree first. Post-land CI runs on the GitHub mirror, and a
  failure triggers an automatic revert. The auto-revert is part of this
  decision, not a later phase.
- **Rejected:**
  - `direct` alone. A broken change reaches `main` before any check has run
    on it; the auto-revert only limits how long it stays.
  - `queued` on `main`. It tests the rebased candidate, but it serializes
    lands and adds minutes of latency.
- **Not covered:** `release/*` lines. No decision is recorded for them.
- The merge-queue rule for rustTorrent changes at cutover (G3.10).

## D7. Hash function

**Decided (operator, 2026-10-22): the recommendation below.**

- **BLAKE3** (**Recommended**). The fastest on large inputs and on many cores,
  with built-in key derivation for domain separation and a verified-streaming
  tree mode for later chunking.
- SHA-256. More conservative and widely recognised. About 3 to 5 times slower
  without SHA extensions.
- git SHA-1 or SHA-256 object format. It would tie the core to git, which the
  operator ruled out.

It is frozen by golden test vectors in G1.1. Changing it later means
rewriting every id.

## D8. GitHub mirror identity

**Decided (operator, 2026-10-22): a GitHub App installed on the org.**

The mirror pushes PR branches (phase 2) and, in graft-primary mode, `main`
itself (phase 3), which needs to bypass the merge-queue ruleset.

- **Chosen:** a GitHub App installed on the org. Short-lived installation
  tokens, its own identity in audit logs, and it can be a ruleset bypass
  actor without being a person.
- **Rejected:**
  - a machine user with a PAT (simpler, but a long-lived credential tied to a
    seat);
  - the existing `thedancingdeveloper` token (it mixes the operator's identity
    with automation).

The operator creates the App and stores its private key in Infisical. The
mirror reads the key from there and mints installation tokens itself. No
graft code holds a long-lived GitHub credential.

## D9. GitHub change notification

- **Poll `git ls-remote` every 15 s** (**Recommended for phase 1**). No
  ingress, and good enough for a read pilot.
- **Webhook** through Caddy (a public route to the mirror). Lag is under a
  second, but it needs a public endpoint and a webhook secret.
- Both: a webhook with a polling backstop. The phase 2 target, if lag matters
  for the PR bridge.

## D10. Agent token issuance

**Decided (operator, 2026-10-22): Vogt mints per-session graft tokens.**

- **Chosen:** Vogt mints per-session graft tokens. Vogt already knows the
  session, its work item and its repos. The token carries `session` and
  `work_item`, so every changeset is attributable. It needs a small
  Vogt-side change, plus a manifest entry (`GRAFT_TOKEN … ondemand`).
- **Rejected:**
  - one static token per repo in Infisical (phase 1 only; no per-session
    attribution);
  - OIDC from an estate IdP (there is no estate IdP for agents today).

What Vogt does:

- At session start it mints a session-scoped token through `MintToken`. The
  token is never broader than Vogt's own token: its scopes are a subset, and
  its expiry is no later.
- While the session lives, it renews the session's workspace lease.
- When the session stops, it closes the workspace. The drafts become
  abandoned and are kept for the retention window (design §8.6).

Phase 1 (read pilot) uses a static read token per repo. The Vogt-side work is
the second half of G2.9.

## D11. Path rules on import

**Decided (operator, 2026-10-22): the recommendation below.**

Git allows any bytes in paths. graft requires UTF-8, and it can enforce
portability (no names that differ only by case, no Windows-reserved names).

- **Reject and report** (**Recommended**). The import stops and lists every
  offending path. The repository is fixed in git first. rustTorrent is
  expected to be clean.
- Escape non-UTF-8 bytes into a reversible encoding. Lossless, but the escaped
  names leak into tools.
- Allow raw byte paths in the model. Every API then carries bytes, not
  strings. This adds baggage for an edge case.

## D12. Repository namespaces

**Decided (operator, 2026-10-22): the recommendation below.**

- **One server, many repos, a separate object namespace per repo**
  (**Recommended**). Simple authorization (the repository is the read
  boundary), simple GC, simple deletion. We lose dedup across repos, which is
  negligible for us.
- A shared object namespace across repos. Dedup, but every object read then
  needs a "does this repo reference it" check, and GC becomes cross-repo.
- One server process per repo. The simplest isolation, but more operational
  work for every new repo.

## D13. Deployment

**Decided (operator, 2026-10-22): the recommendation below.**

- **graft-server and graft-mirror as a Komodo stack on Node B; graft-node as
  a systemd unit on each host that runs sessions** (**Recommended**).
  Production deploys go through Komodo per estate rules. The node daemon needs
  a host socket (`/run/graft/node.sock`) that every session's sandbox can
  reach.
- Everything in containers, including the node daemon. That is possible, but
  the socket has to be bind-mounted into every session container.
- node-e instead of Node B. Less shared load, but further from where most
  sessions run today.

## D14. Pilot repository

**Decided (operator, 2026-10-22): the recommendation below.**

- **rustTorrent** (**Recommended**). Real, active, and agent-heavy. The
  OpenScion trial gives a baseline (git timings, history shape: 296 commits, 9
  merges, symlinks, executables).
- vogt. Busier and more valuable, but a pilot failure there costs more.
- A synthetic repository only. Safe, but it proves nothing about real history.

## D15. Where graft's source lives

**Decided (operator, 2026-10-22): GitHub `TheDancingDeveloper-org/graft`,
public.** Public repositories can use the GitHub merge queue on the Team plan,
so graft lands through the standard merge queue. The analysis below is kept
for the record.

The brief keeps the repository local, with no remote, for now. Before workers
start on phase 1 it needs a home:

- **Forgejo `indexarr/graft`, private** (**Recommended**). Estate-internal,
  with Woodpecker CI.
- GitHub `TheDancingDeveloper-org/graft`, private. GitHub Actions and the merge
  queue, but private repositories on the Team plan have no merge queue (as
  with rdpapp).
- Both, with a mirror. Only if it is published.

## D16. Line-level merge in rebase

**Decided (operator, 2026-10-23): on for text files under 1 MiB, with merged
paths recorded in `graft.merged-paths`. Larger or binary files conflict at the
file level.**

- **On for text files under 1 MiB; merged paths recorded in
  `graft.merged-paths`** (**Decided**). It cuts conflicts sharply for
  agents editing the same file in different places. The metadata lets
  `attested` policy and review notice merged results.
- The other two options are rejected.
- File-level only (Scion's documented behaviour, and OpenScion's plan). The
  simplest, but every same-file pair conflicts.
- Off by default, opt in per land. Safe, but agents rarely opt in.

## D17. MCP hosting

- **Both** (**Recommended**). `graft mcp` over stdio inside a working copy
  (with workspace tools), and `/mcp` on the server for agents without a
  checkout (Vogt oversight, review and bisect bots).
- stdio only. Simpler, but checkout-less agents need a local binary and
  configuration.
- Server only. No working-copy tools.

## D18. Phase 1 kill criteria

**Decided (operator, 2026-10-22): the exit and kill criteria in
[plan.md](plan.md#phase-1-exit-and-kill-criteria), unchanged.**

[plan.md](plan.md#phase-1-exit-and-kill-criteria) proposes the thresholds. The
operator should confirm them, or tighten them, before the pilot starts, so the
verdict is not argued after the fact.

## D19. Import identity

**Decided (operator, 2026-10-22): a provenance key on every imported
changeset.**

rustTorrent has 4 commits that differ from other commits only in the git
committer line (rebased copies). A changeset id doesn't include a committer,
so without a key each pair collapses into one changeset, and the merge joining
them can't be re-exported to its git sha.

- **Chosen:** every imported changeset carries metadata
  `mirror.git-commit = <40-hex sha>`. It's set only by `graft-git`, is never
  written into exported commits, and the core model stays git-free.
- **Rejected:**
  - a digest key only on collisions (inconsistent provenance);
  - a committer field in the core model (git baggage, and it would change the
    pinned changeset encoding);
  - accepting the 4 commits as lossy (fails the 100% fidelity exit criterion).

## D20. No pilot period

**Decided (operator, 2026-10-22): no pilot period. This replaces the gate in
[D18](#d18-phase-1-kill-criteria).**

- G1.18, the 7-day rustTorrent read pilot and its go or no-go verdict, is
  dropped.
- Phase 2 starts as soon as each item's dependencies are done. It doesn't wait
  for a phase 1 verdict.
- The phase 1 exit criteria in
  [plan.md](plan.md#phase-1-exit-and-kill-criteria) become targets: the G1.16
  harness measures and reports them, but nothing is held on them. The
  follow-lag and agent-adoption criteria need a long-running deployment, so
  they are tracked once the G1.17 stack is deployed rather than scored up
  front.
- Cost: the kill criteria no longer stop phase 2. If graft's reads turn out
  not to pay their way, phase 2 work may be wasted.

## D21. Batch PR export

**Decided (operator, 2026-10-23): the near-term way of working. Agents land
on a graft integration line forked from the followed GitHub `main`; each
batch reaches GitHub as one squashed PR, which the operator merges.**

- `graft-mirror pr-batch <line> --base main` follows `main`, syncs the
  integration line onto it, and exports the line's net change as one commit
  on GitHub's current `main`. It pushes `graft/batch-<n>` with a lease and
  opens a PR whose body lists every land in the batch (changeset, author,
  session, work item, attestations) and ends with `Graft-Batch` trailers.
  graft never merges or queues the PR.
- Sync rebases the line's net change onto the moved base (line merge, D16)
  and records it on the integration line as a fast-forward merge
  changeset. A conflict is reported with its paths, and nothing moves or is
  forced.
- When follow imports the PR's merge commit, the batch is recorded as
  merged and the next batch counts from its tip. A PR closed unmerged marks
  the batch rejected, and the line is left as it is, so its lands go in the
  next batch.
- GitHub CI runs once per batch, not once per land. `main` stays
  git-primary: graft does not write it (§11.3, mode A).
- Cost: GitHub's history shows one commit per batch. The per-land detail
  lives in graft and in the PR body.
- The other options are rejected:
  - One PR per land (the G2.6 bridge on every land). CI runs per land, and
    the operator reviews each one.
  - Graft-primary `main` with direct export (mode B). It needs the authority
    switch and ruleset bypass first, and the operator gives up the merge
    button.
