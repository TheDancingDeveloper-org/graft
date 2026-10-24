# graft architecture (diagrams)

This is the visual companion to [design.md](design.md). It shows four things:

1. The code layout: crates and how they depend on each other.
2. The components at runtime, and how they talk to each other.
3. The deployment topologies, including CI.
4. What an **attester** is, and how graft fits with GitHub, with and without CI.

The diagrams are Mermaid, which GitHub renders. Where this page and the design
disagree, the design wins.

> **Phase note ([D22](decisions.md#d22-phase-1-is-a-git-alternative)).** In
> phase 1, lines are `direct` and nothing runs CI in the land path. The
> attester and attestation flows in section 4 are phase 2, built but switched
> off. Topology 3b (graft-primary `main` with GitHub as the mirror) is not
> planned; GitHub delivery is the batch PR of 3c (D21).

---

## 1. Code layout

graft is one Cargo workspace with 19 crates under `crates/`. They form layers.
A crate depends only on crates in the same layer or a lower one.

```mermaid
flowchart TB
  subgraph L0["Foundation"]
    model["graft-model<br/>ids, changesets, trees,<br/>diff, merge3, blame"]
    telemetry["graft-telemetry<br/>tracing, metrics"]
  end

  subgraph L1["Wire and storage contracts"]
    proto["graft-proto<br/>gRPC services, types,<br/>protocol header"]
    store["graft-store<br/>store traits: objects,<br/>metadata, journal, leases"]
  end

  subgraph L2["Storage backends and domain logic"]
    storefs["graft-store-fs<br/>object frames on disk, LRU"]
    storesql["graft-store-sqlite<br/>metadata, journal,<br/>protection records"]
    auth["graft-auth<br/>PASETO tokens, scopes,<br/>jti denylist, CI OIDC exchange"]
    index["graft-index<br/>line and path indexes,<br/>log and blame"]
    gc["graft-gc<br/>orphan dry run, sweep"]
    git["graft-git<br/>deterministic git import and export"]
  end

  subgraph L3["Engine"]
    engine["graft-engine<br/>CreateChangeset, Rebase, Sync,<br/>Land + policies, Attest, Revert,<br/>Stack, line merge"]
  end

  subgraph L4["Processes and clients"]
    server["graft-server<br/>gRPC + /v1 JSON facade,<br/>MCP endpoint, admin CLI"]
    node["graft-node<br/>local edge cache and forwarder"]
    client["graft-client<br/>sync, sparse working copy,<br/>land, conflict files"]
    mirror["graft-mirror<br/>follow, export, batch PR,<br/>PR import, reconcile, authority"]
    mcp["graft-mcp<br/>MCP tools for agents"]
    lsp["graft-lsp<br/>editor integration"]
    cli["graft-cli<br/>the graft command"]
  end

  testkit["graft-testkit<br/>test servers, pilot, crash harness"]

  proto --> model
  store --> model
  storefs --> store
  storesql --> store
  auth --> proto
  index --> store
  gc --> store
  git --> proto
  engine --> auth & index & gc & store
  server --> engine & storefs & storesql & mcp & auth
  node --> auth & storefs
  client --> storefs & proto
  mirror --> client & git
  lsp --> client
  cli --> client & lsp & mcp
  testkit -.-> store
```

Which crate does what:

| Crate | Owns |
| --- | --- |
| `graft-model` | Pure types and algorithms: changeset and tree ids, diff (histogram), `merge3`, blame |
| `graft-proto` | The gRPC API (`RepoService`, `WorkspaceService`, `ObjectService`, `AdminService`, `CiService`) and the `graft-protocol` version header |
| `graft-store` | Store traits. Everything above them is backend-agnostic |
| `graft-store-fs`, `graft-store-sqlite` | Object bytes on disk; metadata, journal and leases in SQLite |
| `graft-auth` | Tokens, scopes (`read`, `write`, `land:<line>`, `attest`, `admin`), and the CI OIDC → graft token exchange |
| `graft-engine` | All repository semantics. The server is a thin shell around it |
| `graft-gc` | Finding orphans and collecting them (deletion is off per repo by default) |
| `graft-git` | Lossless, deterministic git ↔ graft conversion |
| `graft-mirror` | Keeps a git remote (GitHub) and graft in step (§3) |
| `graft-server` | The network process |
| `graft-node` | Optional per-host cache that forwards to the server |
| `graft-client`, `graft-cli`, `graft-lsp`, `graft-mcp` | How people, editors and agents use graft |

---

## 2. Components at runtime

```mermaid
flowchart LR
  subgraph Users["Users"]
    human["Developer<br/>graft CLI / editor + LSP"]
    agent["AI agent<br/>graft CLI or MCP"]
  end

  subgraph Host["Developer or agent host"]
    nodec["graft-node<br/>(optional cache)"]
  end

  subgraph Server["graft-server"]
    api["gRPC API<br/>+ /v1 JSON (read-only)<br/>+ /mcp"]
    authz["auth: token check,<br/>scopes, jti denylist"]
    eng["graft-engine"]
    pol["Land policy gate<br/>phase 1: direct<br/>(attested / queued: phase 2, off)"]
    gcs["GC sweep"]
    meta[("SQLite<br/>metadata, journal,<br/>leases, attestations")]
    objs[("object store<br/>(filesystem frames)")]
    api --> authz --> eng
    eng --> pol
    eng --> meta & objs
    gcs --> meta & objs
  end

  subgraph Mirror["graft-mirror"]
    follow["follow (import)"]
    export["export (push with lease)"]
    bridge["batch PR / PR import"]
  end

  subgraph CI["CI (outside graft; phase 2, off by default)"]
    attester["Attester<br/>runs checks, posts Attest"]
  end

  gh[("GitHub repo")]

  human --> nodec --> api
  agent --> api
  human -. direct .-> api
  follow -- "ImportChangesets" --> api
  export -- "Watch journal" --> api
  gh -- "fetch / webhook" --> follow
  export -- "git push --force-with-lease" --> gh
  bridge <--> gh
  bridge <--> api
  attester -- "ExchangeCiToken, then Attest" --> api
  attester -- "reads the draft" --> api
```

The server is the only writer to its stores. Everything else is a client:
people, agents, the mirror and the attester. Each authenticates with its own
token and scopes. In phase 1 there is no attester: the policy gate passes
every `direct` land, and nothing outside the server takes part in a land.

### One land, step by step (phase 1: `direct`)

```mermaid
sequenceDiagram
  autonumber
  participant A as Agent (graft CLI)
  participant S as graft-server
  participant M as graft-mirror

  A->>S: open workspace, commit (draft changeset)
  Note over S: draft is held by the workspace
  A->>S: land
  S->>S: rebase onto tip (+ line merge, merge3)
  alt clean
    S->>S: CAS move of the line
    S-->>A: Landed
    S-->>M: journal RefMoved(line)
  else conflict
    S-->>A: Conflict {paths, tip}, nothing moves; the client syncs and resolves, as with git
  end
  Note over M: pr-batch sends the line to GitHub as a batch PR, one commit per land (D26); land-pr rebase-merges it (D28)
```

### One land with an attested policy (phase 2, off by default)

```mermaid
sequenceDiagram
  autonumber
  participant A as Agent (graft CLI)
  participant S as graft-server
  participant T as Attester (CI)
  participant M as graft-mirror
  participant G as GitHub

  A->>S: open workspace, commit (draft changeset)
  Note over S: draft is held by the workspace
  S-->>T: new draft (Watch, or a git branch via the mirror)
  T->>S: ExchangeCiToken (CI OIDC JWT) → short attest-only token
  T->>T: build + test the draft
  T->>S: Attest {draft, ci/build, pass}, Attest {draft, ci/test, pass}
  A->>S: land
  S->>S: policy gate: attested? checks present and passing?
  S->>S: rebase onto tip (+ line merge), CAS move of main
  S-->>A: Landed
  S-->>M: journal RefMoved(main)
  M->>G: push the exported commit to main (with lease)
```

---

## 3. Deployment topologies

### 3a. Today's dev topology (graft-dev VM, contained trial)

```mermaid
flowchart TB
  subgraph Prod["vogt-prod (agent host)"]
    vogt["Vogt oversight"]
    w1["worker 1<br/>graft CLI"]
    w2["worker 2<br/>graft CLI"]
    w3["worker 3<br/>graft CLI"]
    vogt --> w1 & w2 & w3
  end

  subgraph VM["graft-dev VM (trigproxmox, VM 110, 4 vCPU)"]
    srv["graft-server :17888"]
    mir["graft-mirror (follow rustTorrent)"]
    att["trial attester<br/>(profile: attester,<br/>off: phase 2)"]
    runner["GitHub runner<br/>label graft-dev"]
    etc[("/etc/graft<br/>keys + tokens, 0600")]
    mir --> srv
    att --> srv
    srv --- etc
  end

  ghsrc[("GitHub: graft repo")]
  ghrt[("GitHub: rustTorrent")]
  ghci["graft CI<br/>(self-hosted runners on Node B)"]

  w1 & w2 & w3 -- "tailnet" --> srv
  ghrt -- "follow (read-only)" --> mir
  ghsrc -- "PR checks / merge queue" --> ghci
  ghsrc -- "merge to main → deploy-dev.yml" --> runner
  runner -- "deploy/up.sh" --> srv
```

- GitHub isn't written. `rusttorrent` follows GitHub read-only.
  `rusttorrent-trial` is graft-primary with no export.
- Under D22 the trial line `trial-1` is `direct`, and the attester profile
  is off. When it is on (phase 2), it stands in for CI, because the trial
  runs no GitHub Actions.

### 3b. Target topology: graft-primary with GitHub as the mirror

```mermaid
flowchart TB
  subgraph Clients["People and agents"]
    dev["developers"]
    agents["agents (Vogt workers)"]
  end

  subgraph GraftHost["graft host"]
    srv["graft-server"]
    mir["graft-mirror<br/>export + PR import"]
  end

  subgraph GitHub["GitHub"]
    repo[("repo<br/>main: written only by the mirror App")]
    prs["Pull requests<br/>(humans, Dependabot)"]
    gha["GitHub Actions<br/>pre-land: graft/* branches → Attest<br/>post-land: on main pushes"]
  end

  dev & agents --> srv
  srv -- "journal" --> mir
  mir -- "export main (lease)" --> repo
  mir -- "export drafts as graft/* branches" --> repo
  repo --> gha
  gha -- "OIDC → ExchangeCiToken → Attest" --> srv
  prs -- "imported as github-pr/N drafts" --> mir
  mir -- "land via graft; PR closes as merged" --> prs
```

### 3c. Batch mode: an integration line, one batch PR with one commit per land (D21, D26, D28)

The near-term way of working. `main` stays git-primary and graft follows it.
Agents land on a graft integration line forked from graft's `main`.
`graft-mirror pr-batch <line> --base main` sends the line's lands to GitHub
as one batch PR carrying one commit per land (D26). Oversight reviews the PR
as a whole and `land-pr` rebase-merges it once GitHub's required checks pass
(D28). GitHub CI runs once per batch.

```mermaid
sequenceDiagram
  participant A as agents
  participant S as graft-server
  participant M as graft-mirror pr-batch
  participant G as GitHub
  participant O as oversight (land-pr)
  A->>S: land on integration (many times)
  M->>G: follow main (ls-remote, fetch, import)
  M->>S: sync: Rebase the line's net change onto main; CAS the line
  M->>M: one commit per land on GitHub's main (deterministic)
  M->>G: push graft/batch-n (lease: must not exist)
  M->>G: open the PR (lands, sessions, work items, Graft-Batch)
  O->>G: review; rebase-merge once required checks pass (graft never merges)
  M->>G: follow imports the merged commits
  M->>S: batch recorded as merged; the next batch counts from its tip
```

- **Sync.** When `main` moved, the line's net change since the base it last
  absorbed is rebased onto `main`'s tip (line merge, D16). A merge
  changeset (first parent the line tip, second `main`'s tip) moves the line
  by compare-and-swap, so the move is a fast-forward. A conflict is
  reported with its paths, and nothing moves.
- **One commit per land.** `graft/batch-<n>` is a chain of commits on
  GitHub's `main`, one per land in land order (sync changesets skipped),
  each with the land's author and message and `Graft-Changeset`,
  `Graft-Session`, `Graft-Work-Item` and `Graft-Checks` trailers. The last
  commit's tree equals the integration tip's tree. A batch carries at most
  500 per-land commits; a longer line is cut into consecutive batches.
- **Squash is the fallback.** When replaying the lands onto the base does
  not reproduce the integration tip, the batch is exported as one squashed
  commit, and the PR body and the commit say why.
- **Every land is listed.** The PR body lists as many lands as fit under
  GitHub's body limit, and names the commit messages for the rest.
  Listings stop at the newest 5,000 lands.
- **Review and merge.** Oversight reviews the batch PR as a whole. For a
  head named `graft/batch-<n>`, `land-pr` waits for the required checks and
  rebase-merges past the squashing merge queue (`--admin`), keeping the
  per-land commits. graft never merges, queues or approves the PR.
- **One batch open per line.** While its PR is open, a run only reports it.
  A PR closed unmerged marks the batch rejected, and its lands stay on the
  line for the next batch.
- **Retries are safe.** The commit chain is built from recorded fields and
  each land's own time, and stored before it is pushed, so a re-run pushes
  the same shas and adopts the PR already on the branch.

---

## 4. The attester, and GitHub with or without CI

### What an attester is

An **attestation** is a signed statement that a named check passed or failed
for one exact draft changeset. For example: "`ci/test` passed for changeset
`c7f…`, log at URL".

An **attester** is anything that runs checks and posts attestations. It holds
an attest-only, short-lived token pinned to that one changeset. It can't
write code, land, or attest any other changeset.

The server never runs builds. It only **checks** attestations at land time,
and only when the line's policy asks for them. So graft works with any CI, or
with none.

| Land policy on a line | Needs an attester? | GitHub equivalent |
| --- | --- | --- |
| `direct` | No. Lands at once; optional post-land CI and auto-revert | A repo with no required status checks: anyone with write can merge a PR |
| `attested` | Yes. Lands only when the required checks are attested for that draft | Branch protection with **required status checks** |
| `queued` | Yes. The candidate is tested after rebase, before the ref moves | GitHub **merge queue** |

Run vanilla GitHub without CI, and the graft equivalent is `direct`. Nothing
needs an attester. The attester exists only to make `attested` and `queued`
work, the same way required checks need a CI system to report them.

### Where attestations come from, by setup

```mermaid
flowchart LR
  subgraph None["No CI (vanilla GitHub)"]
    p1["line policy: direct"] --> l1["land: immediate"]
  end

  subgraph GHA["GitHub Actions"]
    d2["draft exported as<br/>graft/* branch"] --> w2["pre-land workflow<br/>(ci/github/graft-pre-land.yml)"]
    w2 --> x2["graft ci exchange<br/>(OIDC → attest token)"] --> a2["graft attest --check … --pass"]
  end

  subgraph Self["Self-hosted / no GitHub"]
    d3["draft in graft"] --> w3["attester service<br/>(e.g. trial attester)"] --> a3["Attest"]
  end

  a2 & a3 --> gate["server policy gate<br/>attested: all required checks pass?"]
  gate --> land["land"]
```

### Concept map: GitHub to graft

| GitHub | graft | Notes |
| --- | --- | --- |
| Branch + pull request | Workspace + draft stack | No branch per change; a workspace holds the drafts |
| "Merge" button | `graft land` | Rebase onto tip, line merge, CAS move, no merge commits |
| Required status checks | `attested` policy + attestations | Posted by CI through `Attest` |
| Merge queue | `queued` policy | Tests the rebased candidate before the move |
| Status check from Actions | Attestation from an attester | Pinned to one changeset, short attest-only token |
| Branch protection "only X can push" | Line authority + ruleset | In graft-primary, only the mirror App writes GitHub `main` |
| Reviews / CODEOWNERS | Path rules (`require_human_review`) | Per path, per line |

### Who is the source of truth, and what GitHub users can still do

graft runs a line in one of two modes. The mode decides whether GitHub users
change anything.

```mermaid
flowchart TB
  subgraph A["Mode A: git-primary (GitHub is the truth)"]
    a_gh["GitHub main"] -- "follow" --> a_gr["graft main (read-only copy)"]
    a_ag["agent: graft land"] -- "batch PR: graft lands on an integration line,<br/>its net change goes up as one PR" --> a_gh
    a_hu["human: normal GitHub PRs,<br/>merge button, Actions"] --> a_gh
  end

  subgraph B["Mode B: graft-primary (graft is the truth)"]
    b_gr["graft main"] -- "export (lease)" --> b_gh["GitHub main (mirror, read-only to people)"]
    b_ag["agents / devs: graft land"] --> b_gr
    b_hu["human opens a GitHub PR"] -- "imported as github-pr/N draft" --> b_gr
    b_gr -- "landed → PR closes as merged" --> b_hu
  end
```

- **Mode A, git-primary (phases 1–2).** Nothing changes for GitHub users. They
  raise and merge PRs as usual, with or without Actions. Agents read through
  graft, which is fast and sparse. When an agent lands, graft opens a normal
  GitHub PR, so GitHub's checks and merge rules still decide. graft needs no
  attester here.
- **Mode B, graft-primary (phase 3).** graft decides what reaches `main`.
  GitHub users can still open PRs. The mirror imports each one as a draft,
  and it lands through graft's policy:
  - under `direct`, at once;
  - under `attested`, once checks are attested, by Actions or another
    attester.

  The PR then closes as merged. What they can't do is merge on GitHub
  directly. A ruleset keeps `main` writable only by the mirror, because two
  writers would fork history. If someone bypasses that, the mirror stops
  exporting and raises a divergence alert; it never force-pushes.

Moving between modes uses the `graft-mirror authority` steps: freeze, catch
up, verify, flip, unfreeze. Each step can be run in reverse to roll back.
