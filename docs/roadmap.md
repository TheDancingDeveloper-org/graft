# graft roadmap

- **Status:** adopted by the operator on 2026-10-26
  ([D35](decisions.md#d35-the-roadmap-of-record-and-the-phase-2-trigger)).
- **Companions:** [design.md](design.md) (how it works),
  [plan.md](plan.md) (the engineering waves and the backlog),
  [decisions.md](decisions.md) (why), [status.md](status.md) (what is
  delivered).

graft is source control for codebases worked on by many short-lived agent
sessions at once. This page says where it is going and in what order. It is
written for someone deciding whether to adopt graft, so it names the
principles first and the releases second.

## Principles

**The pull request is a bridge, not a primitive.** git has no pull request;
forges invented it for humans sending patches to other humans, with hours
between turns. graft keeps the pull request only at its git boundary
(`graft-mirror`), for as long as a repository's authoritative copy is on a git
host. Nothing inside graft depends on one existing. When a repository's
authority moves to graft, the pull request goes with the bridge.

**A pull request bundles things that do not need to travel together.** graft
pulls them apart:

| What a pull request does | graft primitive | Release |
| --- | --- | --- |
| Integrates a change | `Land`: server-side rebase, line merge, compare-and-swap | v0.1 |
| Verifies a change | attestations on a changeset, posted after the land by a separate checker | v0.2 |
| Authorises a change | line and path policy, satisfied by attestations of kind `review` or `approval` | v0.3 |
| Records who and why | changeset metadata (author, kind, session, work item) and the journal | v0.1 |
| Notifies and queues | the journal and `Watch` | v0.1 |
| Shows a diff to a person | the API, the LSP, and from v0.3 a read-only web UI | v0.1, v0.3 |
| Hosts the conversation | not graft. The tracker or review tool the user already has, reading graft's API | — |

**The agent's contract never changes.** Across every release the loop is
clone, commit, sync, land. What changes is only what `land` can answer:
`Landed`, `Conflict`, `Contended`, and from v0.3 a `Rejected` that names the
missing attestation and who can supply it. An agent never opens anything,
waits on a page, or polls a forge.

**graft runs no build, test or check.** Verification is a separate system
that reads the journal and posts attestations. graft stores the results and
enforces policy on them. Which runner performs a check is the user's choice,
per repository, and can change without graft noticing
([D37](decisions.md#d37-checks-are-decoupled-from-the-forge)).

**graft depends on no particular orchestrator or tracker.** Session and work
item identity are generic fields carried in tokens, metadata and export
trailers. Any tool that can call an API can consume the journal, post an
attestation, or render a diff. The estate that develops graft uses its own
tracker for this; nothing in graft requires it
([D39](decisions.md#d39-independence-from-any-orchestrator)).

**Humans keep their gates, as policy.** Where people want a person in the loop
before a change lands, graft expresses it as a requirement on a line or a
path, satisfied by a human's attestation. The gate is server-side; the agent
sees only `Rejected` with the reason, and the land completes on its own once
the attestation arrives ([D36](decisions.md#d36-parked-lands)).

## Releases

### v0.1: a git alternative with low friction (shipped 2026-10-26)

Correct storage and history; sparse clone; many working copies per host;
workspaces and draft stacks; server-side rebase with line merge and lock-free
land; conflicts handed back in the files; a client-side pre-land conflict
preview; lines, merge points, pick and tags; CLI, MCP and LSP; git import,
follow and export; a local repository mode with no server; one binary; a
zero-config dev server; `graft login` and clone by URL; releases to install.
Delivery to a git host is a batch pull request with one commit per land,
appended to while open.

Measured as agent friction and conflict churn, not read latency
([D31](decisions.md#d31-phase-1-measures-agent-friction-not-read-latency)).
Lines are `direct`; nothing runs in the land path.

What remains in v0.1.x is polish from the professionalisation review:
the user docs set, the bench chart, trust and security pages, a user-facing
compose file, the demo recording, and the orchestrator-neutral naming of
session and work-item fields (D39).

### v0.2: verification without gates

Everything needed to know whether a landed change is good, with nothing in
the land path. This is the work that was deliberately kept out of v0.1, and
it is the precondition for any repository leaving its git host.
The first candidate repository is graft itself
([D40](decisions.md#d40-graft-is-the-first-graft-primary-repository));
it stays git-primary until v0.4.

- **Attestations generalised.** `Attest { changeset, kind, status, by, url }`
  with `kind` in `check`, `review`, `approval`; `GetAttestations` by changeset
  or by line range; every attestation journalled. The record and the RPC exist
  today for `check`; the kinds and the query are new.
- **Post-land checks on candidate lines**
  ([D25](decisions.md#d25-post-land-checks-on-the-batch-line), design §8.7).
  A separate checker watches `RefMoved`, fetches the landed snapshot, runs the
  repository's required checks, posts attestations, bisects when it is
  behind, and lands a revert of the first bad changeset. Land latency is
  unchanged. Only lines configured for it; everything else stays `direct`.
- **A review queue as a query, not a page.** Lands on a line since an LSN
  with no `review` attestation, filterable by path and size, over the API,
  the CLI and MCP. A reviewer, human or model, works the queue from whatever
  tool they already use and posts a verdict as an attestation.
- **The CI token exchange** (built, switched off): a CI job trades its own
  identity token for a short graft token scoped to `attest`, so any runner
  can post results without an admin credential.
- **Journal webhooks.** An outbound, filtered subscription to journal events
  for consumers that cannot hold a `Watch` stream open.

Exit: on a candidate repository every land is checked within a bounded time,
a planted bad land is reverted without a person, and a reviewer works a
day's lands without opening the git host.

### v0.3: gates that people demand, as policy

The `attested` policy switched on where wanted, with a small policy language
per line and per path.

- **Requirements.** `require` lists per path glob and per change size, for
  example: changes under `deploy/**` need `review:human` and
  `check:deploy-dry-run`; changes over 400 lines need `review:human`;
  `release/*` lines are `queued`.
- **Who may attest.** Path rules name the principal kinds or groups whose
  attestation counts, enforced by the `attest` scope. A model may post
  `review:model`; only a person may post `review:human`.
- **Parked lands** ([D36](decisions.md#d36-parked-lands)). A land that is
  `Rejected` for a missing attestation can be parked on the server. When the
  attestation arrives the server completes it: rebase onto the current tip,
  compare-and-swap, done. The agent's session may already have ended. This is
  what replaces the pull request for the human-gate case, and it keeps one
  integration primitive.
- **Proposals as a query.** Drafts and parked lands waiting on attestations,
  with what they wait for and who can give it. CLI, MCP, API, and the web UI.
- **A read-only web UI in the server**
  ([D38](decisions.md#d38-a-read-only-web-ui-in-the-server)). Browse a
  snapshot, log, diff, blame, the review queue and the proposals, served from
  the server over its JSON facade. It can post an attestation (that is a
  write to graft's own state). It holds no conversation and no review
  threads; those stay in the user's tracker.
- **Break glass.** An admin override that lands without the required
  attestations, journalled, with a reason, and visible in the review queue.

Exit: a change under a guarded path cannot land without a person; the person
approves from the CLI, the UI or their own tool in one action; the parked
land completes without the agent; every bypass is in the journal.

### v0.4: the first authority switch

One repository's authoritative copy moves from its git host to graft
(design §11.3, mode B). GitHub `main` becomes an export mirror written only
by the mirror's identity. The batch pull request retires for that repository.
Pull requests opened by people or bots on the git host are imported as drafts
and land through `Land`. Post-land checks and auto-revert are live. The review
queue is the review surface. This is the first release in which a repository
has no pull requests at all.

Where that repository's checks run is its own choice
([D37](decisions.md#d37-checks-are-decoupled-from-the-forge)): the git
host's runners on the export-mirror push, attesting back through the token
exchange, or runners triggered from the journal. Nothing in graft changes
either way.

Exit: two weeks with no divergence between graft and the mirror, no lost
land, and reviews happening outside the git host.

### v0.5 onward: breadth

More repositories switch, one at a time. A per-line land combiner for heavy
contention; path read restrictions; a second metadata store for replicas; a
regional cache tier; a symbol index for the LSP; virtual checkouts; chunked
large blobs. Git support becomes export-only for each repository that has
switched.

## The git track

Git support runs in parallel with every release and has its own lifecycle per
repository:

| State | What graft does | Releases |
| --- | --- | --- |
| git-primary | import, follow, batch pull requests with one commit per land | v0.1 to v0.3 |
| graft-primary | export with a lease on every land, import external pull requests as drafts | v0.4 |
| export-only | export for people who still read the git host; nothing flows back | v0.5 onward |

Rules that hold throughout: git concepts live only in `graft-git` and
`graft-mirror`; no feature in v0.2 or later may depend on a pull request
existing; the full export is rehearsed on a schedule so graft is never a trap.

## The gates, concretely

| What people ask for | How graft expresses it | Release |
| --- | --- | --- |
| A person reviews large changes before they land | a size rule requiring `review:human`; the land parks until attested | v0.3 |
| A person approves anything touching deploy or migrations | a path rule on `deploy/**` plus a `check:deploy-dry-run` attestation | v0.3 |
| Releases are signed off | `release/*` lines with the `queued` policy plus `review:human` | v0.3 |
| Someone looks at what the agents landed today | the review queue, sampled or filtered by risk, verdicts as attestations, no gate | v0.2 |
| Compliance evidence | the journal plus attestations, exportable per changeset | v0.2 |
| Break glass | a journalled admin override, surfaced in the review queue | v0.3 |

## Where checks run

graft does not care, and the roadmap does not name a runner migration as a
milestone. A repository on a git host keeps that host's runners for its batch
pull requests until it switches. A switched repository chooses: keep the git
host's runners on the export-mirror push and attest back, or run checks from
the journal on its own infrastructure. Release builds that need platforms a
user does not host (macOS, signed artefacts) stay wherever they run today.
Runners are retired when nothing needs them, decided per repository, never as
a roadmap item.

## Where a graphical interface fits

Until a repository switches, its git host remains the browser for files,
history and diffs. The day it switches that browser is gone, so v0.3 ships a
read-only web UI in the server: browse at a changeset, log, diff, blame, the
review queue and the proposals, plus the ability to post an attestation. It
holds no conversation. Discussion, decisions and review threads belong in
whatever tracker or review tool the user runs, reading graft's API. graft
never grows review state beyond attestations.

## Reading this page

Each release lists its exit criteria and the decisions that shape it. The
engineering breakdown, crate by crate, is in [plan.md](plan.md); the backlog
is in the operator's tracker and summarised in [status.md](status.md). A
release ships when its exit criteria are met, not on a date.
