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
  graft integration line, and each batch reaches GitHub as one batch PR.
  Amended by D26 and D28: the PR carries one commit per land, and oversight's
  `land-pr` rebase-merges it once the required checks pass.
- **D22** (operator, 2026-10-23): phase 1 is graft as a git alternative.
  Lines default to `direct`, and no build, test or CI runs in the land path.
  CI checks (attestation, the attester, the CI token exchange, post-land
  runs and auto-revert) move to phase 2: the code stays, switched off. D6
  becomes the phase-2 setting for `main`, not the default. See
  [D22](#d22-phase-1-is-a-git-alternative).
- **D23** (operator, 2026-10-23): line fork points. A new line writes one
  fork row and reads below it fall through to the parent line. No data
  migration (the schema bumps to version 5).
  See [D23](#d23-line-fork-points).
- **D24** (operator, 2026-10-23): draft overlap is gated by a new scope,
  `repo:<r>:drafts`, granted only by `admin`. See
  [D24](#d24-draft-overlap-visibility).
- **D25, D26** (operator, 2026-10-24, from the WI-2336 fit assessment):
  post-land checks on the batch integration line, the phase-2 design (it
  starts only with the graft-primary authority switch, and D22 stands in
  full until then); and
  batch PRs that carry one commit per land, revising D21. See
  [D25](#d25-post-land-checks-on-the-batch-line) and
  [D26](#d26-one-commit-per-land-in-a-batch-pr). graft keeps no review state;
  review stays on the GitHub batch PR.
- **D27** (operator, 2026-10-24): `graft land` runs the conflict preview and
  the draft-overlap check by default and stops on a real conflict. See
  [D27](#d27-pre-land-check-in-graft-land).
- **D28** (operator, 2026-10-24, WI-2351): batch PRs get oversight's
  standard review and are merged by `land-pr` once GitHub's required checks
  pass. The operator no longer holds the merge button. Amends D21 and D26.
  See [D28](#d28-batch-prs-are-reviewed-and-landed-like-any-pr).
- **D29** (operator, 2026-10-24, WI-2363): a merge between lines records a
  merge point per (source, target) pair, read by `MergeBase`; history stays
  first-parent. See [D29](#d29-merge-points-between-lines).
- **D30** (operator, 2026-10-24, WI-2434): a land that arrives while its
  batch PR is open and unsealed is appended to it as one more per-land
  commit, a fast-forward with a lease; no close and recut. Amends D21 and
  D28. See [D30](#d30-a-land-after-pr-batch-joins-the-open-batch-pr).
- **D33** (operator, 2026-10-25, WI-2443): an empty changeset with fewer
  than two parents is valid only when it carries `mirror.git-commit` from a
  git import. Native empty changesets stay rejected, and a native write may
  not set `mirror.*`. See [D33](#d33-empty-imported-changesets). Amended by
  [D34](#d34-canonical-empty-root): exactly one canonical empty root is a
  native exception.
- **D34** (operator, 2026-10-25, WI-2366): exactly one canonical empty root
  changeset is valid. No parents, no changes, fixed fields (no author, time
  or randomness), so its id is a constant every new repository shares.
  Other native empty changesets stay rejected. Amends validity rule 9 and
  D33's native-empty rejection for that one changeset only. See
  [D34](#d34-canonical-empty-root).
- **D6, D16** (operator, 2026-10-23): `attested` landing on `main`, with
  post-land CI and auto-revert on failure (D6 is the phase-2 setting under
  D22, not the default); line-level merge in rebase for text
  files under 1 MiB. Each section below names the rejected options.
- **D35 to D39** (operator, 2026-10-26): [roadmap.md](roadmap.md) is the
  product roadmap of record; the pull request is confined to the git
  bridge; phase 2 starts with the decision to switch a named repository;
  parked lands; checks decoupled from the forge; a read-only web UI in
  v0.3; independence from any orchestrator (generic session and
  work-item names).

| # | Decision | Recommendation | Blocks |
| --- | --- | --- | --- |
| D1 | Name | **Decided:** keep `graft`; publish only `graft-*` crates (never the bare `graft` crate); the binary is `graft` | — |
| D2 | Licence and visibility | **Decided:** public, AGPL-3.0-only | — |
| D3 | Storage backend | **Decided:** SQLite + filesystem through phase 3, Litestream for off-box WAL | — |
| D4 | Wire protocol | **Decided:** gRPC (tonic, prost) plus a read-only JSON facade | — |
| D5 | At-rest codec | **Decided:** store the gzip wire frame (zero transcode); benchmark zstd in the pilot | — |
| D6 | Landing gate on `main` | **Decided:** `attested` (pre-land CI on the draft) plus post-land CI with auto-revert on failure. **Phase 2 under D22**: opt-in config, not the default | — |
| D7 | Hash function | **Decided:** BLAKE3 | — |
| D8 | GitHub mirror identity | **Decided:** a GitHub App on the org, with ruleset bypass on `main`; key in Infisical | — |
| D9 | GitHub change notification | Poll every 15 s in phase 1; add webhooks in phase 2 if lag matters | Phase 1 (G1.12) |
| D10 | Agent token issuance | **Decided:** Vogt mints a session-scoped graft token, renews the lease, closes the workspace at stop | — |
| D11 | Path rules on import | **Decided:** reject non-UTF-8 and non-portable paths, with a report | — |
| D12 | Repository namespaces | **Decided:** one server, many repos, a separate object namespace per repo | — |
| D13 | Deployment | **Decided:** a Komodo stack on Node B; the node daemon as a systemd unit per host. Dev server since moved to the graft-dev VM, deployed from `deploy/` (operator, 2026-10-23, WI-2251) | — |
| D14 | Pilot repository | **Decided:** rustTorrent | — |
| D15 | Where graft's source lives | **Decided:** GitHub `TheDancingDeveloper-org/graft`, public | — |
| D16 | Line-level merge in rebase | **Decided:** on for text files under 1 MiB, recorded in metadata; larger or binary files conflict at file level | — |
| D17 | MCP hosting | Both: `graft mcp` (stdio) and the server's `/mcp` | Phase 1 (read tools), Phase 2 (write tools) |
| D18 | Phase 1 kill criteria | **Decided:** as in plan.md, unchanged. Written for the read pilot; targets only under D20 and D22 | — |
| D19 | Import identity | **Decided:** `mirror.git-commit = <sha>` metadata on every imported changeset | — |
| D20 | Pilot period | **Decided:** none; G1.18 dropped, phase 2 not gated (replaces D18's gate) | — |
| D21 | Batch PR export | **Decided:** agents land on a graft integration line; each batch goes to GitHub as one squashed PR the operator merges; amended by D26, D28 (one commit per land; `land-pr` rebase merge) | — |
| D22 | What phase 1 is | **Decided:** graft as a git alternative; lines `direct` by default; CI checks are phase 2, switched off | — |
| D23 | Line fork points | **Decided:** a new line writes one fork row instead of copying its parent's line index; no data migration | — |
| D24 | Draft overlap visibility | **Decided:** a `repo:<r>:drafts` scope, granted only by `admin`; paths and principals, never contents | — |
| D25 | Post-land checks on the batch line | **Decided (phase 2):** each land on an integration line is checked after it lands and marked good or bad; a bad land is reverted and never batched. Direct lines unchanged. Starts only with the graft-primary authority switch; D22 stands until then | Phase 2 |
| D26 | Commits in a batch PR | **Decided:** one commit per land, keeping author, session and work item; squash only as a fallback; no review state in graft. Revises D21 | — |
| D27 | Pre-land check in `graft land` | **Decided:** conflict preview and draft-overlap check by default; a real conflict stops the land; `--no-check` skips | — |
| D28 | Batch PR review and merge | **Decided:** oversight reviews the batch PR as a whole; `land-pr` rebase-merges it past the squash queue (`--admin`) once GitHub's required checks pass; graft still never merges. Amends D21, D26 | — |
| D29 | Merges between lines | **Decided:** one merge point per (source, target) line pair, written with the land and read by `MergeBase`; first-parent history, no second parent, no encoding change | — |
| D30 | Appending to an open batch | **Decided:** a land after `pr-batch` joins the open, unsealed batch PR as one more commit (fast-forward with a lease); sealed batches take no appends | — |
| D33 | Empty imported changesets | **Decided:** an empty changeset with fewer than two parents is valid only with `mirror.git-commit` (git import); native empty changesets stay rejected, and a native write may not set `mirror.*` | — |
| D34 | Canonical empty root | **Decided:** exactly one empty, parentless root changeset is valid, with fixed content and a constant id shared by every new repository; other native empty changesets stay rejected; D33's git-import exception is unchanged | — |

---

## D1. Name

**Decided (operator, 2026-10-25): keep `graft`.** No rename. Only the
`graft-*` crates are ever published to crates.io, never the bare `graft`
crate (taken by another project); the binary stays `graft`. The analysis
below is kept for the record.

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

**Phase 2 under [D22](#d22-phase-1-is-a-git-alternative).** In phase 1 every
line, `main` included, lands `direct`, and no check runs in the land path.
The choice below is the opt-in phase-2 configuration, not the default.

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
  for the PR bridge. (The per-land PR bridge was later removed under
  D21/D22.)

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

> **Amended by [D26](#d26-one-commit-per-land-in-a-batch-pr) and
> [D28](#d28-batch-prs-are-reviewed-and-landed-like-any-pr) (2026-10-24).**
> The text below is D21 as decided; where it differs, D26 and D28 win. A
> batch PR is no longer one squashed commit: it carries one commit per land,
> in land order, each keeping its author, session and work item, and squash
> is only the fallback when replaying the lands does not reproduce the
> integration tip (D26). The operator no longer merges batches: oversight
> reviews the batch PR as a whole and `land-pr` rebase-merges it (bypassing
> the squashing merge queue) once GitHub's required checks pass. graft
> itself still never merges or queues the PR (D28).

- `graft-mirror pr-batch <line> --base main` follows `main`, syncs the
  integration line onto it, and exports the line's net change as one commit
  on GitHub's current `main`. It pushes `graft/batch-<n>` with a lease and
  opens a PR whose body lists every land in the batch (changeset, author,
  session, work item, attestations) and ends with `Graft-Batch` trailers.
  graft never merges or queues the PR.
- Every land is listed. The squashed commit's message lists all of them.
  The PR body stays under GitHub's 65,536-character limit (graft-mirror
  caps it at 60,000): it lists as many lands as fit and says how many more
  are in the commit message. Each cell is capped, and at most 8 checks are
  shown per land. A batch listing is bounded at 5,000 lands (the newest);
  a larger batch is still exported, and both texts say it has more.
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
    the operator reviews each one. The bridge was removed (WI-2302,
    WI-2320).
  - Graft-primary `main` with direct export (mode B). It needs the authority
    switch and ruleset bypass first, and the operator gives up the merge
    button.

## D22. Phase 1 is a git alternative

**Decided (operator, 2026-10-23): phase 1 is graft as a git alternative.
Lines default to `direct`, and no build, test or CI runs in the land path. CI
checks are phase 2 or later.** Phase 1 is source control: graft runs no
build, test, check or checker anywhere. Phase 2 starts only with the
graft-primary authority switch.

Phase 1 is:

- correct storage and history;
- fast sparse clone and sync;
- workspaces;
- server-side rebase, line merge (merge3, D16) and lock-free CAS land;
- conflicts handed back to the client, as git does;
- the CLI, MCP and LSP;
- git import and export;
- GitHub delivery as one squashed batch PR (D21).

Phase 2 or later covers CI checks:

- the `attested` and `queued` policies;
- the attester;
- the CI token exchange;
- post-land runners and auto-revert.

That code is already built (G3.1, G3.8, the trial attester). It stays in the
tree but is optional and switched off. Nothing in phase 1 extends it,
depends on it or prioritises it until the operator starts phase 2.

Why:

- The rustTorrent trial (WI-2270) spent its time in the attestation path
  (the attester, its queue and its checks), not in source control. It
  measured CI, not graft.
- git works without CI. A git alternative has to stand on storage, history,
  merge and land, so that is what phase 1 measures.
- Scion and OpenScion have no pre-land gate either. They land at once and
  rely on post-merge testing.
- Under D21, GitHub CI already runs once per batch on the squashed PR, and
  the operator merges it. Breakage is caught there, before it reaches
  GitHub's `main`.

What follows from it:

- **Line policy.** Every line, `main` included, defaults to `direct`. D6
  (`attested` on `main` with post-land auto-revert) becomes opt-in
  configuration (`[[lines]]` in `server.toml`), the phase-2 setting. The
  engine default changes to match (WI-2298).
- **Measurement.** graft is measured as source control: land latency and
  contention, merge and conflict correctness, sync and clone speed, history
  integrity, and comparisons against git. The repeatable harness in `bench/`
  (WI-2294) is used rather than live agent runs.
- **Delivery to GitHub.** GitHub `main` stays git-primary (D21). The
  graft-primary cutover of rustTorrent (G3.10) is not planned, and the
  per-land PR bridge (the G2.6 CLI) has been removed (WI-2302, WI-2320).
- **Plan.** [plan.md](plan.md) keeps the build waves as history. Its phase
  table and backlog follow this decision.

The options it replaces:

- Phase 1 as a read pilot (the original plan), with native landing gated
  by CI in phase 3. The read pilot was dropped by D20. Gating every land on
  CI makes land latency CI latency, and that hides the source-control
  properties phase 1 needs to prove.
- Keeping D6 as the default on `main`. Then a server with no config would
  gate lands on checks that nothing posts.

## D23. Line fork points

**Decided (operator, 2026-10-23): creating a line writes one fork row instead
of copying its parent's line index. No data migration; the schema bumps to
version 5.**

- Creating a line on existing history indexes only the unlanded stretch it
  lands. It writes one `line_forks(repo, line, parent, pos)` row (SQLite
  schema version 5) pointing at a line that holds the first landed ancestor
  in its own rows. A line created at the root has no fork row.
- `line_pos` and the path scan read the line's own rows, then fall through to
  the parent at positions `<= pos`, one hop at a time. Positions are
  first-parent depth, so no renumbering is needed.
- Creating a line costs the unlanded stretch plus one indexed read, not
  O(history x paths) rows (AGENTS.md rule 2).
- Consequence: the line index can no longer be rebuilt from refs and history
  alone. Which line a line forked from, and at what position, is stored only
  in `line_forks`.
- No data migration, rollback guard or backfill: graft has no users. Existing
  databases open at version 5 with their rows intact.
- Rejected: shared-prefix segments (rewrites every row's key shape), and
  chunking the full copy (keeps the O(history) cost and breaks one
  transaction per write).

Details: [notes/line-fork-point.md](notes/line-fork-point.md) (WI-2316).

## D24. Draft overlap visibility

**Decided (operator, 2026-10-23): "who else has an open draft on these
paths" is answered only to holders of a new scope, `repo:<r>:drafts`.**
The design is [notes/draft-overlap.md](notes/draft-overlap.md) (WI-2310).

- **Scope.** `repo:<r>:drafts` takes no argument. Only `admin` grants it.
  `read`, `write` and `land` don't, and `drafts` doesn't grant `read`.
  No token gets it by default; `MintToken` adds it when asked.
- **What is visible.** Paths, workspaces, owners, Vogt sessions, work
  items, draft age and lease end. Never file contents, head or changeset
  ids, messages, diffs or tokens.
- **Lifetime.** Only live, leased workspaces count. Closed workspaces drop
  out when they close; expired ones are filtered at read time until GC
  removes their rows.
- **Paging.** `OverlappingDrafts` pages `(path, workspace)` hits in index
  order with an opaque cursor. Each page scans at most 4 × `limit` index
  rows, so its cost is O(limit) however broad the prefix. The empty or root
  prefix is rejected. A page can be short or empty and still carry a token.
- **Index.** The draft-paths index is updated in the same transaction as
  the write that changes a workspace's stack (rule 3).
- **Migration.** None for data: there are no users yet. The new tables and
  the nullable `workspaces.work_item` column come from a schema migration;
  no golden vector moves.

Rejected: gating by `read` (every reader would see every agent's
activity), and scanning workspaces at query time (breaks rule 2).

## D25. Post-land checks on the batch line

**Decided (operator, 2026-10-24) as the phase-2 design: post-land checks for
batch integration lines only. Each land on the line runs the repository's
required checks after it lands and is marked good or bad before it is
batched. A bad land is reverted on the line and never reaches a batch PR.
Every other line is unchanged.** **Phase 2:** this starts only with the
graft-primary authority switch. Until then
[D22](#d22-phase-1-is-a-git-alternative) stands in full, and graft runs no
build, test, check or checker anywhere (operator, 2026-10-24, WI-2349). The
design is [design.md §8.7](design.md#87-post-land-checks-on-the-batch-line).

- **Where.** Only a line with `checks = "post-land"` in its `[[lines]]`
  entry, which `pr-batch` requires of its integration line once the feature
  ships. `direct` lines and the followed base line are unchanged; nothing
  else in phase 2 (`attested`, `queued`) is switched on.
- **Land latency is unchanged.** The land is still rebase, line merge and
  CAS. Checks run after the CAS, on the landed snapshot, so they test the
  line-merged result that pre-land CI never sees (the clean-merging,
  build-breaking land).
- **Which checks.** The checks the target repository requires on GitHub
  `main`, named per line in `server.toml` (`required = ["ci/fmt",
  "ci/clippy", "ci/test"]`). The commands live with the checker, not in
  graft.
- **Who runs them.** The existing attester (`graft-attester`), retargeted
  from drafts to landed changesets, with the attest-only token of G3.8
  (WI-2211, WI-2265). A GitHub Actions runner can post through the same OIDC
  exchange instead. The server still never runs a build.
- **Good and bad.** A land is `good` when every required check passed on
  it, `bad` when one failed after its retry, otherwise `pending`. The mark
  is stored by the attestation's own transaction in an index keyed by land,
  so reading it never scans the journal (rule 2).
- **Bad lands.** The checker lands a `Revert` of the first bad land on the
  line (bisecting when it checked several lands at once). Lands after it
  stay; they are re-checked on top of the revert. `pr-batch` cuts a batch
  only up to the newest land whose prefix is all good or reverted, so one
  bad land no longer sinks a batch.
- **Prerequisite.** WI-2293 (a failed `GetWorkspace` stalls the
  attester's journal read) is fixed first; the post-land checker stands on
  that read.

Why: the fit assessment (WI-2336, gap 1). rustnzb failed 73 of 405
merge-queue runs after per-PR CI passed; vogt failed 15 of 120. Under D21 and
D22 that breakage reaches the batch PR, and a red batch is rejected whole.

Rejected:

- `attested` (pre-land checks) on the integration line. It makes land
  latency CI latency, which D22 rejected, and it checks the draft, not the
  merged result, so it misses the failure that matters.
- `queued` on the integration line. It catches merged breakage but
  serialises every land behind CI.
- Keeping D21's "reject the whole batch" with a manual
  `pr-batch --exclude`. It needs the operator to find the bad land, and
  later lands built on it still carry the breakage.
- Post-land checks on every line. Not asked for; the other lines stay the
  phase-1 measurement of source control.

## D26. One commit per land in a batch PR

**Decided (operator, 2026-10-24): a batch PR carries one commit per land, in
land order, each keeping its author, Vogt session and work item, so the
batch can be reviewed, blamed and bisected per change.** This revises
[D21](#d21-batch-pr-export), which squashed the batch into one commit. The
design is [design.md §11.4](design.md#114-batch-pr-export).

- **Shape.** `graft/batch-<n>` holds a chain of commits on GitHub's
  current `main`: one per land (sync changesets skipped), with the land's
  author, its message, and `Graft-Changeset`, `Graft-Session`,
  `Graft-Work-Item` and `Graft-Checks` trailers. The committer is the
  mirror. The last commit's tree equals the integration tip's tree.
- **Reverted lands.** A bad land and its revert (D25) are both commits, so
  the history says what happened.
- **Deterministic.** Each commit is built from recorded fields and the
  land's own time, and the chain is recorded before the push, so a retry
  pushes the same shas.
- **Merge.** The operator merges with rebase-merge to keep the commits, or
  squash as before. graft still never merges or queues the PR.
- **Fallback.** When replaying the lands onto the base does not reproduce
  the integration tip (a conflict against a moved base, which sync would
  also report), the batch is exported squashed, as D21 did, and both the PR
  body and the commit say why.
- **Bounds.** A batch carries at most 500 per-land commits; a line with more
  lands is cut into consecutive batches. The PR body keeps D21's caps.
- **No review state in graft.** graft adds no review verb, approval or
  review metadata. Review happens on the GitHub batch PR, commit by commit,
  and the operator's merge is the approval. graft serves the diffs (the
  read API) to whatever client does the reviewing.
- **Identity.** It is only useful once lands carry identity: gap 5 of the
  fit assessment (the CLI passes `VOGT_SESSION_ID` and `VOGT_WORK_ITEM`) is
  a separate small item, and should land first.

Why: the fit assessment (WI-2336, gap 2). rustTorrent #214 carried four
lands as one commit, had no review, and waited 17.3 h against a 1.6 h
median.

Rejected:

- One PR per land (the removed G2.6 bridge): CI and review per land again.
- Squash only (D21 as written): one review and one blame entry per batch.
- A review record in graft (a review verb or `review.url` metadata, fit
  assessment gap 2). It duplicates what the PR already holds, and graft
  would have to keep it in step with GitHub.
- Exporting the integration line's history as is: its sync merge
  changesets would put graft's merges into GitHub's history and need a
  merge-commit merge.

## D27. Pre-land check in `graft land`

**Decided (operator, 2026-10-24): `graft land` runs the conflict preview
(`status --upstream`) and the draft-overlap check (`status --drafts`) by
default, and stops on a real conflict. `--no-check` skips them.** The design
is [design.md §8.8](design.md#88-pre-land-check-in-graft-land).

This is separate from D25: it runs in the client, before the land, on every
line, and runs no build or test. D22's "no build, test or CI in the land
path" still holds.

- **Real conflict.** A file the stack and the line tip both changed that
  line merge (D16), under the land's `--merge` mode, cannot merge. Files
  that both sides changed but that merge cleanly are reported and don't
  stop the land.
- **Draft overlap** is reported, never a stop. It needs the
  `repo:<r>:drafts` scope (D24) and is skipped without it.
- **Dry run.** The preview uses `Rebase { dry_run: true }`, a new flag
  (an interface PR in `graft-proto`) that runs the line merge and writes
  nothing: no changeset, pin or journal entry. A plain `Rebase` would leave
  a changeset and pins behind on every land.
- **Advisory.** The server's land is still the authority. A tip that moves
  after the check is caught by `Land` as before. No server state and no
  policy change.
- **Clients.** graft has no LLM interface. The check belongs to the client
  surfaces (the CLI, the MCP `land` tool), and the same result is in the
  JSON output, so every client gets the same behaviour.

Why: the fit assessment (WI-2336, §4.2). Several clients often work on the
same files. Today they find a conflict only when the land fails, and they
see who else is on those paths only if they ask.

Rejected:

- Opt-in (`--check`). Clients that don't know to ask never get the warning.
- A server-side gate. It adds state and a policy for something the CAS
  already decides.
- Stopping on draft overlap. An overlap is not a conflict, and the other
  draft may never land.

## D28. Batch PRs are reviewed and landed like any PR

**Decided (operator, 2026-10-24, WI-2351): a batch PR goes through
oversight's standard Claude review and is merged by oversight's `land-pr` once
GitHub's required checks pass, the same as every other reviewed PR.** This
amends [D21](#d21-batch-pr-export) ("graft never merges or queues the PR; the
operator merges") and [D26](#d26-one-commit-per-land-in-a-batch-pr) ("the
operator's merge is the approval"). The design is
[design.md §11.5](design.md#115-batch-pr-review-and-merge).

- **graft still never merges.** Oversight merges, through `land-pr`,
  outside graft. graft does not queue, approve or merge the PR.
- **No per-change review.** The batch PR is reviewed as a whole by a
  read-only reviewer. graft stores no review data and calls no LLM (D26).
- **Blockers name the land**, by its commit (D26). They are fixed forward
  by a new land on the integration line or by dropping the land; an open
  batch is not rewritten.
- **Merge shape: batch PRs bypass the merge queue** (operator, 2026-10-24,
  WI-2361). The queue squashes, which would collapse the per-land commits of
  D26. So for a head named `graft/batch-<n>`, `land-pr` waits for the
  required checks on the PR head, then runs
  `gh pr merge --rebase --admin --match-head-commit <sha>` and asserts the PR
  is MERGED. `pr-batch` keeps the `graft/batch-<n>` branch name, which is what
  `land-pr` keys on.
- **Ordinary PRs still squash through the queue.** Workers' feature and docs
  PRs are unchanged: they land through the merge queue, and a worker whose
  branch conflicts still runs `git merge origin/main` (never a rebase or
  force-push).
- **Checks are the forge's.** GitHub's required checks run on the PR as
  they do on any PR. graft runs no build, test or CI (D22 holds).
- **PR body.** `pr-batch` ends the body with how the PR is merged.

Why: rustTorrent batch #214 got no review and waited 17.3 h for the
operator's merge, against the repository's 1.6 h median (WI-2336, gap 2).

Rejected:

- The operator merges each batch (D21 as written): the wait above.
- A review step inside graft: duplicates the PR and needs an LLM in graft.

## D29. Merge points between lines

**Decided (operator, 2026-10-24, WI-2363): a merge from one line into
another records a merge point for that (source, target) pair, and
`MergeBase` reads it. History stays first-parent: no second parent and no
change to the frozen changeset encoding.** The design is
[design.md §8.9](design.md#89-merges-between-lines).

- **What is stored.** Each merge landing appends one `line_merges` row
  (SQLite schema version 7; version 6 is the draft path index): the source
  line, the target line, the source changeset and its position, and the
  target position the merge landed at.
  The latest row for a pair is that pair's merge point.
- **One transaction.** The row is written in the land's CAS transaction,
  with its journal entry (`LineMerged`). Only the target ref moves; the
  source is read, never moved (AGENTS.md rules 3 and 4).
- **`MergeBase` reads it.** Between two lines it returns the newest target
  point that the source already contains, following fork rows (D23) and
  merge rows a bounded number of hops. So a repeated back-merge starts from
  the last merge, and a merge whose earlier merges reached the source by
  another line (git flow's release/1.1 → main) is clean, as in git.
- **Surface.** `LineMerge` in the API; `graft line merge <src> [--into
  <dst>]` in the CLI; `graft_merge` in MCP. A conflict is handed back like a
  land conflict, and the client lands its resolution with `--merged-from`,
  which records the same row.
- **Costs follow the change.** Reading a merge point is an indexed read per
  hop, capped. Nothing walks history or scans the journal.
- **Git export.** A merge landing exports as a one-parent commit with a
  `Graft-Merged-From:` trailer. No git merge commits.

Why: the branching-strategies trial (WI-2362). Carries between lines are
squashes with no record, so `merge_base` never advances: release/1.1 → main
met 11 conflict blocks that git does not, repeated back-merges re-conflict,
and each carry cost 7 to 22 round trips against git's 2.

Rejected:

- A second parent on a landed changeset. It changes the frozen changeset
  encoding and makes the line index, blame, export and fork rows deal with
  non-first-parent history.
- Merge metadata only in the changeset (`graft.merged-from`) read by
  `MergeBase`. Finding it means scanning a line's changesets, which breaks
  costs-follow-the-change; the metadata is still written, for readers.

## D30. A land after pr-batch joins the open batch PR

**Decided (operator, 2026-10-24, WI-2297; design WI-2434): a land that
arrives on the integration line while its batch PR is open is appended to
that PR as one more per-land commit (D26), instead of waiting for the next
batch.** This amends [D21](#d21-batch-pr-export) and the batch rule of
[D28](#d28-batch-prs-are-reviewed-and-landed-like-any-pr) ("a fix-forward goes
in the next batch; an open batch is not rewritten"): the open batch is still
never rewritten, but it may grow. The design is
[design.md §11.6](design.md#116-appending-to-an-open-batch).

- **When.** The line's last batch is open (PR open, not merged, not
  rejected), not sealed, and the line has moved only forward from the
  batch's cut: every changeset after the cut is a land (no sync changeset).
  While a batch is open the run does not sync, so this holds unless the line
  was moved by hand.
- **Sealed.** A batch is sealed once its merge has started: the PR has
  auto-merge enabled, is in a merge queue, or GitHub reports it merging.
  Then nothing is appended, and new lands wait for the next batch, as
  before. `land-pr`'s `--match-head-commit` (D28) closes the race: an append
  that wins against a merge in flight makes the merge fail on the head, and
  oversight reviews again.
- **Push.** The new commits are built on the batch's last commit, replayed
  as D26 replays (line merge onto the commit before, same base), root-checked
  against the new line tip, recorded, and then pushed as a fast-forward of
  `graft/batch-<n>` with a lease on the old head. Never a force push. The
  earlier commits keep their shas.
- **Review.** The appended commits are reviewed again. GitHub drops stale
  approvals on a new push, and oversight re-reviews the whole PR head, with
  the new commits named. graft keeps no review state (D26).
- **Drop and cap.** `--exclude` of a land in the open batch still lands a
  revert, and the revert is appended like any land: no close and recut. A
  batch that would pass `max_lands` or `MAX_COMMITS` (500) is not appended
  to; the lands wait. `max_age_ms` holds back only the first cut, never an
  append.
- **Fallback.** When an append does not replay cleanly or fails the root
  check, nothing is pushed and the batch is left as it is; the lands wait for
  the next batch, and the run says why. An open batch is never re-exported
  squashed.
- **Attribution.** Each appended commit has the land's author, session and
  work item trailers and `Graft-Batch: <n>`, exactly as D26. The PR body's
  land table is rewritten to list every commit.

Why: trial 2 (#215 to #216) and trial 3 (#217 to #218, #164 to #165,
#1190 to #1191). Every review fix-forward cost a close, a recut and a fresh
review of commits that had not changed.

Rejected:

- Fixes wait for the next batch (D28 as written): the known bug merges first.
- Close and recut on every fix (what the trials did): the review restarts
  and CI runs again on unchanged commits.
- Rewriting the batch branch (amend the bad land's commit, force push): it
  breaks the lease rule and the determinism of D26.
- An opt-in `pr-batch --append`: a fix-forward that needs a flag gets
  forgotten; appending is the default and `--no-append` turns it off.

## D31. Phase 1 measures agent friction, not read latency

**Status:** accepted (operator decision, 2026-10-25, review WI-2445).

**The phase-1 MVP target is minimal agent friction and multi-session building without conflict churn.** Speed is not that target. It is two guards:

1. **No regression.** No read path (`log`, `status`, `sync`) is slower than the medians recorded by WI-2341 on 2026-10-24 (bench smoke in the merge queue). Those medians, graft / git, p50 ms: at 1k, `log -50` 3.3 / 2.0, `status` 4.1 / 3.2, `sync` 54.6 / 23.4; at 10k, `log -50` 3.4 / 1.9, `status` 13.3 / 6.5, `sync` 70.6 / 29.1.
2. **Rule 2.** Every operation is O(change), never O(history). A query that scans the journal or every changeset is a bug even when it is fast.

**2× faster than git is a later-phase target**, not a phase-1 exit or kill criterion. WI-2400 (read-path parity with git) is `wont_do`; the floor above replaces it. WI-2341 is done: its medians are the floor, and its open question is answered.

**The `.graft/index` fsync stays.** Dropping it would trade durability for milliseconds, and the gaps against git are 1–40 ms per command. The estate's cost was conflict churn, not latency.

**Why the old bar is withdrawn.** The earlier read-path parity bar, and the 2× kill criterion, were inherited from the read pilot ([D18](#d18-read-pilot-first), [D20](#d20-no-pilot-period)). Phase 1 is the git alternative ([D22](#d22-phase-1-is-a-git-alternative)), and its success is whether concurrent sessions land without churn. Exit and kill criteria are restated in [plan.md](plan.md#phase-1-exit-and-kill-criteria).

**Unchanged.** D22 stands. Phase 2 (CI, D25) still starts only with the graft-primary authority switch. graft runs no build, test, check or checker anywhere in phase 1.

**Wording.** graft does not remove conflicts on registration lists, moves or lockfiles. It reduces conflicts through line merge, and surfaces the rest in the agent's own session instead of as a DIRTY PR or a queue ejection hours later.

## D32. Local repositories

**Decided (operator, 2026-10-25, WI-2366): a local repository is a directory
holding the SQLite metadata and the object tree, and the CLI opens the
engine in-process when the working copy's server is `local`. The
"clients are online" non-goal stands for remote repositories only.** The
design is [design.md §5.4](design.md#54-local-repositories).

- **Storage.** One directory: `meta.sqlite` (the server's schema and
  migrations, `graft-store-sqlite`) and `objects/` (`graft-store-fs` frames).
  No new encoding. `graft init [DIR] [--repo NAME]` creates it and a working
  copy whose server is `local:<path>`. `graft clone <path>` opens another
  working copy on the same directory and does not use the network.
- **Opening.** The CLI builds `graft-engine`'s in-process `RepoApi` over those
  two stores, the wiring the server already uses, and runs the command
  against the trait. No HTTP, no node socket, no token. An exclusive lock
  file (`repo.lock`) is held for the command; a second process gets a typed
  busy error. One process at a time replaces leases. The local principal is
  implicit.
- **Unchanged rules.** Compare-and-swap lands, one transaction per logical
  write (with its journal entry), objects durable before metadata names
  them, costs follow the change, GC off by default, typed errors, bounded
  responses. A conflict exits 3. `kill -9` during a land loses no
  acknowledged write.
- **Same commands.** Commit, sync, land, log, diff, blame, stack, revert,
  pick, tag and line. `graft import` into it and `graft mirror export` out of
  it. `graft mcp` on stdio uses the same trait object. The same binary still
  clones a remote server when the server value is an endpoint.
- **Not local.** No node daemon, no HTTP, no `/mcp` over HTTP, no tokens, no
  ACL, no multi-user. Writing to a remote repository while offline stays
  unsupported.
- **Promotion.** Moving a local repository onto a server (`graft
  push-to-server`) is a follow-up. This decision does not define it. The
  working copy switches to a remote only by changing its server value, which
  does not copy history.

Why: first-run friction. Today trying graft needs a server, a mirror, a
secrets directory and a minted token. Raised from a future-phase note to p1
on 2026-10-25.

Rejected:

- A local cache that queues writes and reconciles later. That is the
  offline-first model this decision keeps out, and it needs a second
  consistency story.
- A long-running local daemon. The point is no extra process. The lock file
  is enough for one user.
- A new store or a new crate. The server's sqlite and filesystem stores
  already are the engine's ports.

## D33. Empty imported changesets

**Decided (operator, 2026-10-25, WI-2443): an empty changeset with fewer
than two parents is valid only when its metadata carries `mirror.git-commit`.**
That covers a single-parent commit and a parentless root. Native
`CreateChangeset` with no changes and fewer than two parents stays rejected.
This is an exception to validity rule 9 in
[design.md §4.5](design.md#45-changesets). The canonical encoding does not
change, and the golden vectors for native changesets are unchanged.

rustnzb `main` has four single-parent commits whose tree is byte-identical
to the parent's (`1443f1c2b16d527a0bb3345f8ad81a1c31f8da50`,
`0951bf9507873a632742f7ffc27739f4dabdf37f`,
`6bca7aecd7131ab4780d4b5eb3bf1b802a06da9d`,
`48c2c30bd8768eef4c6672eee1cbf58adf23fb72`). Import precheck reports
`empty-changeset` and writes nothing, so the follow never creates a ref.
An empty change list is already a legal encoding for a merge
(`parents.len() >= 2`). Accepting it for one parent does not change the
bytes of ids, trees, changesets, frames or tokens. It does change the
validity rule consumers enforce.

- **Chosen:** `Changeset::check` allows zero changes with fewer than two
  parents only when metadata contains `mirror.git-commit` (D19). The native
  write boundary rejects any metadata key under `mirror.*`, including
  `mirror.git-commit`, so only `graft-git` on import can set it. Export
  already reproduces an empty diff from an empty change list plus the stored
  raw headers, so `export(import(c)).sha == c.sha` holds. Native requests
  without the key still fail `Rule::EmptyChangeset`, and a native request
  that supplies `mirror.*` fails at the boundary before that rule.
- **Rejected:**
  - splicing the empty commits out of parent links and keeping only the raw
    commit in the mirror (lossy against the complete DAG and against 100%
    fidelity; every descendant's git parent changes);
  - rewriting the four commits upstream before the follow can proceed;
  - treating `--allow-lossy` as covering them (it only names commits whose
    export sha does not match, and these never get that far).

The model and importer change is a later PR. This decision does not
implement it.

## D34. Canonical empty root

**Decided (operator, 2026-10-25, WI-2366): exactly one empty, parentless
root changeset is valid.** It has no parents and no changes. Every other
field is fixed: no author, no time and no randomness, so nothing in the id
comes from the server, the clock or a random source. Its id is therefore a
constant, and every new repository starts from that same well-known root.
This amends validity rule 9 in [design.md §4.5](design.md#45-changesets)
and narrows D33's rejection of native empty changesets to every empty
changeset except this one. D33's git-import exception is unchanged: an
empty changeset with fewer than two parents is still valid when metadata
carries `mirror.git-commit`. The canonical encoding does not change.
Existing golden vectors are unchanged. The code PR that implements this
adds one new golden vector for the canonical root; this decision does not.

- **Fixed content.** Parents empty, changes empty, root the empty tree,
  author the fixed empty identity (empty name, empty email, kind `bot`),
  `authored_at` the epoch (`0` ms, offset `0`), message empty, metadata
  empty. The empty tree is the tree of no entries
  ([design.md §4.4](design.md#44-trees)): canonical payload the version byte
  alone, id `Tree::default().id()` =
  `td0a89364d3af4e3469f3f4f545b3fb47ef2c72f9f2daddeedbd1821d18a4234a`, the
  value `golden_empty_tree_id` already pins and the value `empty_tree()`
  returns in `graft-engine` (`source`) and `graft-client` (`fetch`). A
  second parentless empty changeset, or this shape with any field
  different, is not the canonical root and stays rejected.
- **Git export skips this changeset**
  ([design.md §11.2](design.md#112-export)). It is never written as a git
  commit. Its children export as parentless git root commits, so export
  never emits `author <> 0 +0000` and never needs a land record the root
  does not have. No other changeset is omitted.
- **Git import does not start at this root.** An imported repository keeps
  its imported roots. Import does not insert the canonical root above them.
  D33's exception for empty imported changesets is unchanged.
- **Round trip.** Import of a repository that was exported from `graft init`
  does not restore the canonical root as a parent. The first real commits
  come back as imported roots. That loses no file content and no message:
  the skipped changeset has an empty tree and an empty message. Re-import
  does not special-case them back onto the canonical root. `export(import(c))`
  for a commit that was imported from git is unchanged, because those
  histories never contained this changeset.
- **`graft init` (D32).** Creating a local repository writes this one
  changeset and points the line at it. The working copy's first commit is a
  child of that root. Two repositories created by `graft init` share the
  root id before either has a real commit. A remote repository created
  empty uses the same root; this decision does not add a second root shape.
- **Still rejected.** A native empty changeset with a parent, a native
  parentless changeset that is not this fixed content, and any native write
  that sets `mirror.*`. An empty change list with two or more parents stays
  a merge, as before.
- **Rejected:**
  - a fresh root id per repository (author, time or a random nonce in the
    content), which would make two new repositories incomparable at the root
    and put server time or randomness in an id;
  - allowing every native empty changeset, which D33 kept closed;
  - changing the encoding or regenerating the existing golden vectors.

The model change and `graft init` writing this root are later PRs. This
decision does not implement them.

## D35. The roadmap of record, and the phase-2 trigger

**Decided (operator, 2026-10-26): [roadmap.md](roadmap.md) is the product
roadmap of record. graft's development continues away from the pull request:
the pull request is a git-interop artefact confined to `graft-mirror`, kept
for as long as a repository's authority is on a git host, and nothing inside
graft may depend on one existing. Phase 2 (v0.2, verification) starts with
the decision to switch a named repository to graft-primary, taken at the end
of v0.1; it builds, on candidate lines only, what that switch needs. Nothing
runs in the land path of a git-primary line at any point.**

- **What the pull request did, and what replaces it.** Integration is
  `Land`. Verification is attestations posted after the land by a separate
  checker (v0.2). Authorisation is line and path policy satisfied by
  attestations (v0.3). Provenance is changeset metadata and the journal.
  Notification is `Watch`. The diff view is the API, the LSP and the v0.3
  web UI. The conversation is not graft's: it lives in whatever tracker or
  review tool the user runs, reading graft's API.
- **The agent contract is fixed.** Clone, commit, sync, land, in every
  release. `Land` gains only a more informative `Rejected` in v0.3. An agent
  never opens a proposal, polls a forge or waits on a page.
- **The trigger, sharpened.** D22 said phase 2 "starts only with the
  graft-primary authority switch". Read literally that makes the switch
  impossible, because the switch needs post-land checks first. The trigger
  is the operator's decision to switch a named repository. From that
  decision, v0.2 and v0.3 build the verification and gate primitives for
  that repository's lines, switched off everywhere else, and v0.4 performs
  the switch. The no-checks rule for git-primary lines is unchanged.
- **Releases ship on exit criteria, not dates.** Each release in roadmap.md
  names its own.

Why: a reviewer asked why agents should care about pull requests. They
should not. The estate's own practice already shows it: reviews were posted
to the tracker, the merge was done by a script, and the pull request was a
CI-and-merge vehicle whose costs (a red batch rejected whole, a fix-forward
that could not join, hours waiting on a button) were all forge-shaped.
Anthropic's account of Scion kept review and dropped the pull request; graft
does the same.

Rejected:

- Keeping the pull request as graft's unit of review after the authority
  switch. It re-imports human pace into an agent loop and ties graft to one
  forge's object model.
- Starting verification only after the switch (the literal reading of D22).
  Circular; see above.
- Building the conversation into graft (review threads, comments). D26
  already rejected review state in graft; attestations are the only review
  state graft holds.

## D36. Parked lands

**Decided (operator, 2026-10-26): a land that is `Rejected` for a missing
attestation can be parked on the server, and the server completes it when
the attestation arrives: rebase onto the current tip, compare-and-swap,
done. The proposal that humans gate is a parked land, not a new object.**
The mechanics are a v0.3 design PR (design §8.3 and a new §8.9).

- **Shape.** `Land { ..., park: true }` (or a server default per line)
  returns `Parked { land_id, missing: [kind:name...], who: [...] }`. The
  draft stack stays held by a park record, not the workspace lease, so the
  agent's session may end. `GetAttestations` and the proposals query show it.
- **Completion.** When every required attestation for the stack's head is
  present, the server runs the ordinary land: rebase (line merge) onto the
  tip, then CAS. A conflict at completion leaves the park in place with a
  `Conflict` reason; a client (the author's next session, or anyone with
  write scope) resolves it as today. Re-attestation after a rebase follows
  the line's `attest_merged` setting.
- **Lifecycle.** A park expires (default 7 days) into an abandoned draft,
  journalled. The author, an admin, or a principal named by the path rule can
  cancel it. Every transition is a journal entry.
- **Idempotent.** A second `Land` for the same head returns the existing
  park. Completion is one transaction with the land record, as any land.

Why: it keeps one integration primitive, keeps the human gate invisible to
the agent, and gives people a thing to look at (the proposals query, the web
UI) without a pull request.

Rejected:

- A separate `Proposal` object with its own state machine. Two integration
  paths to keep consistent; a pull request by another name.
- Making the agent poll until attested. Human attestation takes hours; an
  agent session does not.
- Pre-land `attested` without parking (the G3.1 shape). It works for CI
  checks that take minutes and fails for people.

## D37. Checks are decoupled from the forge

**Decided (operator, 2026-10-26): where a repository's checks run is the
repository's choice, made when it switches to graft-primary, and can change
later without graft noticing. Runner migration is not a roadmap item.**

- **git-primary (v0.1 to v0.3):** the git host's runners keep running on the
  batch pull request. graft is not involved.
- **Candidate lines (v0.2):** a checker on the user's infrastructure watches
  the journal and posts attestations. The reference checker is
  `graft-attester` in post-land mode (D25).
- **graft-primary (v0.4):** either the git host's runners run on the
  export-mirror push and attest back through the CI token exchange (zero
  migration), or runners are triggered from the journal. Both post the same
  attestations; policy sees no difference.
- **Release builds** that need platforms the user does not host (macOS,
  signed artefacts, code scanning) stay wherever they run today.
- **Retirement** of a runner happens when nothing needs it, per repository,
  never as a milestone.

Why: the decision to leave a forge is expensive and has nothing to do with
source control. Decoupling checks from where they run lets each repository
move at its own pace and keeps graft's surface to attestations.

Rejected:

- A "GitHub-free" release that moves every check onto estate runners. It
  needs capacity (macOS) the estate does not have and couples the roadmap to
  an infrastructure migration.
- Naming a runner in graft's configuration. graft names checks, not runners.

## D38. A read-only web UI in the server

**Decided (operator, 2026-10-26): v0.3 ships a small server-rendered web UI
in `graft-server`, over its JSON facade: browse a snapshot, log, diff,
blame, the review queue and the proposals. It can post an attestation. It
holds no conversation and no review threads.**

- **Why then.** Until a repository switches, its git host is the browser for
  files, history and diffs. The day it switches (v0.4) that browser is gone.
  The UI is built in v0.3 so it exists on the first switch.
- **Scope.** Read paths plus one write: posting an attestation (an existing
  RPC, with the caller's token and scope). No comments, no threads, no
  approvals beyond attestations, no settings. Bounded and paginated like
  every other response (rule 7).
- **Where the conversation lives.** In the user's tracker or review tool,
  reading graft's API. graft never grows review state beyond attestations
  (D26). The estate's tracker is one such consumer and is not special.
- **Not a replacement for the LSP or MCP.** Those stay the surfaces for
  agents and editors.

Rejected:

- No UI, relying on CLI, MCP and LSP. People reviewing an agent's day need a
  browser.
- Putting the UI in the estate's tracker. It would make graft unusable for
  anyone else (D39).
- A full forge (threads, approvals, settings). Scope graft has declined
  since D26.

## D39. Independence from any orchestrator

**Decided (operator, 2026-10-26): graft depends on no orchestrator or
tracker. No user is expected to run or integrate with the estate's tracker.
Session and work-item identity are generic fields; the names that leaked the
estate's tracker into user-visible surfaces are renamed, with the old names
accepted as aliases.**

Validation on 2026-10-26, main at f91ee22: graft has no functional
dependency on the estate's tracker (no crate calls it, imports it or needs it
running). The coupling is vocabulary on user-visible surfaces:

| Surface | Today | Becomes |
| --- | --- | --- |
| CLI environment | `VOGT_SESSION_ID`, `VOGT_WORK_ITEM` | `GRAFT_SESSION_ID`, `GRAFT_WORK_ITEM`; the old names read as a fallback |
| Changeset metadata keys | `vogt.session`, `vogt.work_item`, `vogt.model`, `vogt.coauthors` | `agent.session`, `agent.work_item`, `agent.model`, `agent.coauthors`; the engine's token-matching rule and the LSP hover read both |
| Git export trailer | `Vogt-Work-Item` | `Graft-Work-Item`, beside the existing `Graft-Session`; follow reads both |
| MCP and LSP text | "Vogt sessions", "Vogt work item" | "sessions", "work item" |
| Doc comments and docs | "Vogt session" | "session (set by the caller's orchestrator, if any)" |

- **Token claims** (`session`, `work_item`) are already generic and
  unchanged. The engine's rule that metadata must match a session-bound token
  is unchanged in substance.
- **No encoding changes.** Metadata keys are free-form strings and trailers
  are commit text; existing changesets keep their old keys and are read
  through the alias. No golden vector moves.
- **The estate's deploy docs** (`deploy/`, trial notes) may name the
  estate's tracker, because they describe one deployment. `design.md`,
  `plan.md`, `roadmap.md`, `docs/agents.md`, `docs/guide/` and every
  user-visible string do not.
- **Integration surface for any orchestrator:** mint a scoped token per
  session (`MintToken` under a service principal), set the two environment
  variables, consume the journal through `Watch` or webhooks (v0.2), post
  attestations, and read diffs over the API. Nothing else.

Why: the operator's tracker is a personal tool. A source control system that
presumes it is not a product.

Rejected:

- Leaving the names and documenting them as "estate conventions". Every
  user would see them in `--help`, metadata and git history.
- A hard rename with no aliases. It would break the estate's own sessions
  and the per-commit mapping in follow for already-exported history.
