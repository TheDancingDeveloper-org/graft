# graft-dev: the graft development stack (deploy/)

The graft-dev VM (on trigproxmox, tag `graft`) runs graft's development
server as one docker compose stack, defined here and deployed by a
self-hosted GitHub runner on the same VM (WI-2251). No Komodo, no ops repo,
no Forgejo. The same VM is graft's quiet CI and benchmark host.

| File | What it is |
|---|---|
| `compose.yml` | `graft-server` and the one `graft-mirror` service, hardened (uid 10001, read-only root, `cap_drop: ALL`) |
| `image.env` | `GRAFT_IMAGE`, the image pinned by `@sha256` digest |
| `server.toml` | graft-server config (`max_token_ttl_s = 2592000`, 30 days) |
| `mirror-service.toml` | `graft-mirror serve`: server, token file, data directory, metrics. No repositories: each is a mirror row (D41) |
| `mirror-service-cutover.md` | the one-time move from per-repository mirrors to the service, with the retire steps and rollback |
| `up.sh` | idempotent deploy: bootstrap secrets, pull, config pre-flight, `up -d --wait`, readiness gate with rollback, health check |
| `backup/` | backup script, systemd timer, `install.sh`, `restore-rehearsal.sh` (WI-2307) |
| `monitoring/` | Prometheus alert rules and scrape config for the ops repo (WI-2307) |
| `attester/Dockerfile` | the trial attester's image (profile `attester`, off by default; see below), and target `checker`, the post-land checker's (profile `post-land`, off by default) |

The `deploy-dev` workflow is retired: graft-dev runs on a k8s pod, and a deploy
is a digest bump in the ops repo, `apps/graft-dev/gen.py`. `deploy/up.sh` is for
by-hand use only.

## Backups and alerts

Backups are a systemd timer, installed once with
`sudo deploy/backup/install.sh`; alerts live in the estate Prometheus. See
[docs/runbooks/graft-dev-backup-restore.md](../docs/runbooks/graft-dev-backup-restore.md).

## Ports

Published on `GRAFT_BIND_IP` (default `127.0.0.1`). On the VM, set it to the
VM's Tailscale address in `/etc/graft/deploy.env`, which `up.sh` reads:

```sh
echo "GRAFT_BIND_IP=$(tailscale ip -4)" | sudo tee /etc/graft/deploy.env
```

The runner user must be able to read it (on the VM: 0640 root:runner). If it
exists but is unreadable, `up.sh` stops rather than falling back to loopback.

- `17888` graft-server: gRPC, the `/v1` JSON facade, `/healthz`, `/readyz`, `/metrics`
- `17889` graft-server metrics listener: `/metrics`, `/healthz`, `/readyz`
- `17890` graft-mirror `/metrics`: every followed repository, series labelled `repo`

## Secrets

None in git, in the workflow, or in the compose environment. They are host
files under `/etc/graft` (override with `GRAFT_SECRETS_DIR`), mode 0600, owned
by uid 10001, mounted read-only:

| File | Mounted at | Created by |
|---|---|---|
| `/etc/graft/signing.key` | `/run/secrets/signing.key` | `up.sh`, first run (32 random bytes) |
| `/etc/graft/kek-k1` | `/run/secrets/kek-k1` in `graft-server` | by hand, once (32 random bytes; [cutover](mirror-service-cutover.md) step 1); `up.sh` stops without it and only fixes its mode and owner |
| `/etc/graft/mirror-service-token` | `/run/secrets/graft-mirror-token` in `graft-mirror` | `up.sh`, minted by the running server: kind `mirror`, scope `repo:*:mirror` |
| `/etc/graft/attester-token` | `/run/secrets/graft-attester-token` | `up.sh` with `GRAFT_ATTESTER=1`; owned by root, not uid 10001 (see below) |
| `/etc/graft/github-checker-token` | `/run/secrets/github-checker-token` in `graft-post-land` only | `up.sh` with `GRAFT_POST_LAND=1`, from Infisical, or placed by hand (see below) |
| `/etc/graft/post-land-token` | `/run/secrets/graft-post-land-token` in `graft-post-land` | `up.sh` with `GRAFT_POST_LAND=1`; scopes `read`, `attest`, `write` on `graft` and `land` on the checked line (see below) |

`up.sh` writes them from a throwaway root container of the pinned image, so
the runner user needs only the docker group, not sudo. A missing file stops
the container from starting rather than being created as a directory.

The mirror service token has the stack's maximum TTL (30 days). `up.sh`
re-mints it when it is older than 20 days (`GRAFT_TOKEN_RENEW_DAYS`), or on
`GRAFT_REMINT=1`, and restarts the service. There is no per-repository token
or upstream-credential file: each row's upstream credential is sealed in the
server under `kek-k1`, set with `graft repo mirror credential`.

## Followed repositories (D41)

One `graft-mirror serve` container follows every repository that has a
mirror row, thirteen today. A row holds the upstream, the branch → line map,
the poll interval and the sealed credential; the service lists the rows and
runs one follow task per enabled row. Its state is one volume,
`graft-dev-mirror`, holding `<repo>/mirror.git`, `<repo>/mirror.sqlite` and a
migration marker per repository. `graft repo mirror status` shows each row;
`/metrics` on 17890 carries every repository, with a bounded `repo` label.

### Onboard another repository

The operator runs `graft repo mirror add`, with no change to this directory
and no deploy:

```sh
graft repo mirror add NAME https://github.com/OWNER/REPO.git --follow main=main
graft repo mirror credential NAME https://github.com/OWNER/REPO.git --from-file FILE   # private upstream only
graft repo mirror status
```

The service picks the row up on its next reconcile (15 s) and the first poll
is the initial import. Create the graft repository first if it does not exist
(`graft-server admin repo create NAME`). A private upstream needs the
`credential` step (read from a file, never an argument); a public one needs
none. Hosts outside
`[mirror] allowed_hosts` in `server.toml` are refused.

The per-repository files this replaced (`mirrors.tsv`, `mirror-*.toml`,
`compose.onboarded.yml`, `onboard-repo.sh`, `render-onboarded-compose.sh`,
`private-follow.sh`, the `mirror-token-*` files and the
`GRAFT_DEV_MIRROR_TOKEN_*` deploy secrets) are gone. Session tokens are
minted with `trial/mint-agent-tokens.sh` (D10): one `--kind agent` token per
session, scopes `read`, `write`, `drafts` and `land:<line>`.

### Open a batch pull request

`pr-batch` still takes a per-repository config (`--config`); the service does
not run batches yet (`claim_requests` is off until the batcher does, D41
§3.5). Until then, write the config for the repository with
`graft-mirror init-config` (the retired `mirror-<repo>.toml` files are in git
history before the retirement commit and show the shape) and run it from a
one-off container or a checkout, with a GitHub token file of your own:

```sh
graft-mirror pr-batch <line> --base <branch> --config <file>
```

This is a known gap of the retirement, not a design choice; it ends when
batches become server requests.

### A Forgejo upstream (WI-3492)

A repository whose upstream is on Forgejo is onboarded the same way, with
its Forgejo URL and a Forgejo token as the row's credential; the Forgejo host
must be in `[mirror] allowed_hosts`. git fetches and pushes with that
credential. Name the host in the serve config so batch cuts open their PRs
through Forgejo's API, with the same row credential:

```toml
[[forgejo]]
host = "git.example"                          # as the upstream URLs name it
# api = "https://git.example/api/v1"          # the default
required_contexts = ["ci/woodpecker/pr/test"] # land-batch only
token_file = "/run/secrets/forgejo-token"     # land-batch only
```

Forgejo has no merge queue. Merge a batch PR with
`graft-mirror land-batch <pr> --serve-config <file> --upstream <url>`: one
attempt, a rebase merge only once every required context is `success` (exit
0 merged, 75 pending, 1 failed, 3 closed unmerged). A per-repository config
with `[forgejo]` sends that table's token to git for its own host, and
`land-batch --config` reads it the same way.

Check that the service has caught up (lag 0 for every repository):

```sh
curl -s "http://$(tailscale ip -4):17890/metrics" | grep '^graft_mirror_lag'
docker logs --tail 20 graft-dev-mirror
```

## Trial attester (WI-2276)

The contained trial (WI-2270) runs on the integration line `trial-1` of
`rusttorrent`, the followed copy (decision D21). It gets its `ci/fmt`,
`ci/clippy` and `ci/test` attestations from a small attester on this VM, not
from GitHub. It is the
`graft-attester` service (crate `crates/graft-attester`), in the compose
profile `attester`, and is **off by default**. Turn it on in
`/etc/graft/deploy.env`:

```sh
echo "GRAFT_ATTESTER=1" | sudo tee -a /etc/graft/deploy.env
deploy/up.sh
```

With it on, `up.sh` also:

- mints `/etc/graft/attester-token`, kind `service`, scopes
  `repo:rusttorrent:read` and `repo:rusttorrent:attest`, with the
  stack's TTL and the same renewal as the mirror token (`GRAFT_REMINT=1`
  forces it). `attest` grants `Attest` only, so `read` is needed to watch
  the journal and check drafts out. It needs a server that knows the
  `attest` scope (WI-2272);
- builds the image from this checkout (`deploy/attester/Dockerfile`) and
  starts it.

The repository, the line and the checks are configuration:
`GRAFT_ATTESTER_REPO` (default `rusttorrent`; `up.sh` creates it if the
stack doesn't already serve it) and `GRAFT_ATTESTER_LINE` (default
`trial-1`) in `/etc/graft/deploy.env`, and the `--check=name=command` lines
of the service in `compose.yml`. `up.sh` does not create the line.

To run the WI-2270 trial end to end:

1. `deploy/server.toml` carries the `[[lines]]` entries: since decision D22
   (phase 1, WI-2298) `trial-1` (graft-primary) and `main` (git-primary, so
   the mirror's `main` is unchanged) are both `direct`, and the attester's
   checks gate nothing. To gate `trial-1` on them again (phase 2), set it to
   `attested` on `ci/fmt`, `ci/clippy` and `ci/test` (the file shows how, and
   the commented-out D6 block for `main`). The policy is global to the server (WI-2288), so every
   repository gets these lines. A change to the file restarts the server on
   the next `up.sh`.
2. Put `GRAFT_ATTESTER=1` in `/etc/graft/deploy.env` (the defaults already
   name repo `rusttorrent` and line `trial-1`; override with
   `GRAFT_ATTESTER_REPO` and `GRAFT_ATTESTER_LINE`) and run `deploy/up.sh`.
3. From a machine on the tailnet, `trial/create-trial-line.sh` creates
   `trial-1` at the current tip of the followed base line (it leaves an
   existing line alone), then `trial/mint-agent-tokens.sh N` mints the
   agents' tokens (`read`, `write`, `drafts`, `land:trial-1`; never the base
   line). Both take `GRAFT_TRIAL_REPO` (default `rusttorrent`),
   `GRAFT_TRIAL_BASE` (default `main`) and `GRAFT_TRIAL_LINE`; for vogt, the
   trial line forks from `rust-core`:
   `GRAFT_TRIAL_REPO=vogt GRAFT_TRIAL_BASE=rust-core`.
4. When the agents are done, `graft-mirror pr-batch trial-1` opens the one
   squashed PR (D21).

What it does:

- It reads the journal of `rusttorrent` (`Watch`, long-poll) for
  `ChangesetCreated` entries of drafts held by a workspace that tracks
  `trial-1`. It reads a workspace's line from its `WorkspaceOpened` entry,
  in journal order, and forgets it at `WorkspaceClosed`, so a name closed
  and reopened is read correctly. For a workspace opened before its read
  position it asks `GetWorkspace`. It never remembers a "not found". Drafts
  on `main` or any other line are ignored. It keeps the
  newest draft per workspace, since a land needs the stack's head
  attested. The queue holds at most 16 drafts. When it is full the attester
  stops reading, and the journal holds the rest.
- It checks one draft at a time: it confirms the changeset is still a
  draft, checks its tree out into `/var/lib/graft-attester/work/<draft>`,
  runs rustTorrent's Rust job in order. The checks come from D21, scoped
  as rustTorrent's CI scopes them: the Tauri crate `rtbit-desktop` (it
  needs GTK) is excluded, and only the `default-tls` feature is on:

  | Check | Command |
  |---|---|
  | `ci/fmt` | `cargo fmt --all -- --check` |
  | `ci/clippy` | `cargo clippy --workspace --exclude rtbit-desktop --no-default-features --features default-tls -- -D warnings` |
  | `ci/test` | `cargo test --workspace --exclude rtbit-desktop --locked --no-default-features --features default-tls` |

  It posts `Attest` for each, passed or failed. As in a CI job, a check
  that fails makes the later ones fail without running (`--keep-going`
  runs them all). The toolchain is Rust 1.95 with rustfmt and clippy, as
  rustTorrent's CI pins it. Each check has a
  timeout (`GRAFT_ATTESTER_TIMEOUT_S`, default 2700 s). At the timeout, or
  when the check ends, its whole process group is killed.
- The attested URL is the log's path,
  `file:///var/lib/graft-attester/logs/<draft>/ci-clippy.log`, in the
  `graft-dev-attester` volume. Each log is capped at 8 MiB and kept for 14
  days. To read one:
  `docker exec graft-dev-attester cat /var/lib/graft-attester/logs/<draft>/ci-test.log`.
- The read position is kept in `/var/lib/graft-attester/state/lsn`. A
  restart re-runs at most the drafts that were still queued.
- Every draft that is still a draft gets one attestation per check, never
  silence. A checkout that fails transiently (the server unavailable, the
  token refused, a local I/O error) is retried up to 5 times, then each
  check is attested failed, with a log saying why. A checkout that can never
  work (a tree the client refuses) is attested failed at once. Each `Attest`
  call is retried with backoff. If one still fails, the results are kept and
  the draft goes back to the front of the queue, so the next turn posts only
  what is missing and doesn't run the checks again.

Containment:

- The container is read-only, with `no-new-privileges`, and capped at
  `GRAFT_ATTESTER_CPUS` (2), `GRAFT_ATTESTER_MEM` (6g) and 4096 processes.
  Cargo uses `GRAFT_ATTESTER_JOBS` (2) jobs, so builds leave room for the
  VM's quiet benchmarks (4 vCPU). The trial accepts slower attestations.
- The attester runs as root with only `CHOWN`, `SETUID`, `SETGID` and
  `KILL`. It hands each tree to uid 10001 and runs the checks as that uid,
  with a cleared environment. The token file is root's (0600). The attester
  refuses a token file that group or others can read. It holds the token in
  memory only, and never prints it or passes it to a check. A draft's build
  script or tests therefore can't read the token.
- The cargo registry, the target directory and `HOME` live in
  `graft-dev-attester-cache` and are shared between drafts, for speed. A
  hostile draft could poison what later drafts build against. For the trial,
  whose writers are trusted, this is acceptable. To start clean:
  `docker volume rm graft-dev-attester-cache` (with the attester stopped).
- The checks have network access: cargo fetches crates.

Turning it off: remove `GRAFT_ATTESTER=1`, then
`docker compose --env-file deploy/image.env -f deploy/compose.yml --profile attester rm -sf graft-attester`.
A push that changes only `crates/graft-attester` does not redeploy (the
workflow watches `deploy/**`). Run `deploy/up.sh` or the workflow by hand.

## Post-land checker (v0.2 exit, WI-2604)

The compose profile `post-land` runs `graft-post-land`: `graft-attester
--mode post-land --runner github` on the line `GRAFT_POST_LAND_LINE`
(default `exit-rehearsal`) of `graft`. It is **off by default**; turn it on
with `GRAFT_POST_LAND=1` in `/etc/graft/deploy.env`. It reads its own
GitHub token, `github-checker-token` (fine-grained: graft only, Actions
read/write, Metadata read; Infisical `apps/GRAFT_DEV_CHECKER_GITHUB_TOKEN`),
never the mirrors' `github-token`, and its own `post-land-token`. `up.sh`
places the first from Infisical when the host has the `secret` tool, or uses
the copy placed on the host. Without it the checker stays off. `up.sh` mints
the second. The token
needs `write` on the repository as well as `land` on the line, because a
revert requires write on every path the bad land touched. The procedure,
the measurements, rotation and the switch to `backlog-1` are in
[docs/runbooks/v02-exit.md](../docs/runbooks/v02-exit.md).

## Deploy safety: pre-flight and rollback (WI-2291)

A config can need a newer image than `image.env` pins (after #77,
graft-server refused an unknown `[[lines]]` field and crash-looped). `up.sh`
guards both sides of the swap:

- **Pre-flight.** After pulling the pinned image and before touching the
  running stack, it checks `server.toml` and `mirror-service.toml` with that image's
  own binaries, in throwaway containers with no network and no secrets.
  graft-server loads its config before dispatching, so
  `graft-server --config server.toml admin` exits 78 on a bad config and 64
  (usage) on a good one. graft-mirror has no check command; `serve` with no
  token mounted gets as far as reading the token file only if its config
  parsed. A failure stops the deploy; the running stack is unchanged.
- **Readiness gate and rollback.** After `compose up --wait` it waits up to
  `GRAFT_READY_WAIT` seconds (default 120) for `/readyz`. If the server or
  the stack doesn't come up, it copies the last good `image.env`,
  `server.toml` and `mirror-service.toml` back into the checkout, restores
  the metadata database backup (below) if this deploy took one, brings that
  stack up, and exits non-zero either way.
- **Database backup.** A new server image may migrate the metadata
  database, and an older build refuses a newer schema. So before a server
  image other than the running one starts, `up.sh` stops graft-server and
  copies `meta.db` and its WAL files to `pre-deploy/` on the
  `graft-dev-meta` volume; a rollback restores it before the old image
  starts. Tested by `deploy/tests/meta-backup.sh`. That copy covers only
  the deploy that took it: the next image change replaces it, and a later
  deploy of the old image backs up the already-migrated database, which the
  old image refuses. To roll back after an upgrade deploy succeeded, keep
  the upgrade's `pre-deploy/` off the volume (the cutover's step 8a does)
  and restore it by hand.
- **Last good.** A deploy that passes `/healthz` and `/readyz` records its
  `image.env`, `server.toml` and `mirror-service.toml` under
  `/etc/graft/last-good/` (root-owned, 0644; no secrets). The first deploy
  with this script has none to roll back to, and says so.

When `up.sh` exits non-zero, an `::error::` annotation names the cause.

## Deploy by hand

```sh
deploy/up.sh
```

On a fresh VM this bootstraps everything but the credential key `kek-k1`
(provision it first; `up.sh` stops without it): the signing key, the server,
the repository `graft-dev`, the mirror service token and the mirror service.
It then checks `/healthz`. The mirror service is part of `compose up --wait`
(its `/metrics` health check). Mirrored repositories come with their rows
(`graft repo mirror add`), not from `up.sh`.

`/etc/graft` keeps its own mode (0750 root:runner on the VM); `up.sh` only
writes files inside it. The runner user needs the docker group, not sudo.

To run compose yourself, pass the image file:

```sh
docker compose --env-file deploy/image.env -f deploy/compose.yml ps
```

## Repin the image

1. Cut an image: run the `image` workflow on `main` (workflow_dispatch).
2. Read the digest it printed:
   `gh run view <run-id> -R TheDancingDeveloper-org/graft --log | grep -i digest`
3. Put the `sha256:…` value in `deploy/image.env` and land the change. The
   push to `main` deploys it.

## Moving state from Node B (WI-2251 Part C)

The Node B stack (ops repo `personal/graft-dev`) kept its signing key inside
the `graft-dev-meta` volume. To keep existing tokens valid, copy the volumes'
contents into the VM's volumes of the same names and copy that key to
`/etc/graft/signing.key` (0600, uid 10001) before the first `up.sh`. Without
it, `up.sh` creates a new key, and tokens minted on Node B stop working.

## Mint another token

A human token into a root-only file on the VM, never printed (this is how
`/etc/graft/sprooty.token` was made):

```sh
docker compose --env-file deploy/image.env -f deploy/compose.yml exec -T graft-server \
  graft-server admin token mint --subject sprooty --kind human \
  --scope repo:graft-dev:read --scope repo:graft-dev:write \
  --scope repo:rusttorrent:read --ttl 2592000 \
  | sudo sh -c 'umask 077; cat > /etc/graft/.t && [ -s /etc/graft/.t ] && mv /etc/graft/.t /etc/graft/sprooty.token'
```

Or straight into Infisical:

```sh
docker compose --env-file deploy/image.env -f deploy/compose.yml exec -T graft-server \
  graft-server admin token mint --subject <who> --kind service \
  --scope repo:<name>:admin --ttl 2592000 \
  | secret set GRAFT_DEV_TOKEN --project apps --env <env>
```

The token goes to stdout only. Pipe it straight into its store; never echo it.
