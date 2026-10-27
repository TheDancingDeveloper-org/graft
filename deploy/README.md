# graft-dev: the graft development stack (deploy/)

The graft-dev VM (on trigproxmox, tag `graft`) runs graft's development
server as one docker compose stack, defined here and deployed by a
self-hosted GitHub runner on the same VM (WI-2251). No Komodo, no ops repo,
no Forgejo. The same VM is graft's quiet CI and benchmark host.

| File | What it is |
|---|---|
| `compose.yml` | `graft-server` and `graft-mirror`, hardened (uid 10001, read-only root, `cap_drop: ALL`) |
| `image.env` | `GRAFT_IMAGE`, the image pinned by `@sha256` digest |
| `server.toml` | graft-server config (`max_token_ttl_s = 2592000`, 30 days) |
| `mirror.toml` | graft-mirror follows rustTorrent `main` into repo `rusttorrent` |
| `mirror-fluent2.toml`, `mirror-vogt.toml` | graft-mirror-fluent2 and graft-mirror-vogt follow fluent2-egui `main` and vogt `rust-core` into repos `fluent2-egui` and `vogt` (trial 3) |
| `mirrors.tsv`, `compose.onboarded.yml` | followed-repository registry and generated Compose services |
| `onboard-repo.sh` | one-step, idempotent repository onboarding; `--dry-run` prints the plan |
| `up.sh` | idempotent deploy: bootstrap secrets, pull, config pre-flight, `up -d --wait`, readiness gate with rollback, health check |
| `backup/` | backup script, systemd timer, `install.sh`, `restore-rehearsal.sh` (WI-2307) |
| `monitoring/` | Prometheus alert rules and scrape config for the ops repo (WI-2307) |
| `attester/Dockerfile` | the trial attester's image (profile `attester`, off by default; see below), and target `checker`, the post-land checker's (profile `post-land`, off by default) |

The workflow is `.github/workflows/deploy-dev.yml`: on a push to `main` that
touches `deploy/**`, and on `workflow_dispatch`, it runs `deploy/up.sh` on the
runner labelled `[self-hosted, graft-dev]`. It is not a required check. The image is public on GHCR, so the job does
not log in to a registry.

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
- `17890` graft-mirror `/metrics`
- `17891` graft-mirror-fluent2 `/metrics`
- `17892` graft-mirror-vogt `/metrics`

## Secrets

None in git, in the workflow, or in the compose environment. They are host
files under `/etc/graft` (override with `GRAFT_SECRETS_DIR`), mode 0600, owned
by uid 10001, mounted read-only:

| File | Mounted at | Created by |
|---|---|---|
| `/etc/graft/signing.key` | `/run/secrets/signing.key` | `up.sh`, first run (32 random bytes) |
| `/etc/graft/mirror-token` | `/run/secrets/graft-mirror-token` | `up.sh`, minted by the running server |
| `/etc/graft/mirror-token-fluent2`, `/etc/graft/mirror-token-vogt` | `/run/secrets/graft-mirror-token` in their own mirror | `up.sh`, the same way, each scoped `repo:<its repo>:admin` |
| `/etc/graft/attester-token` | `/run/secrets/graft-attester-token` | `up.sh` with `GRAFT_ATTESTER=1`; owned by root, not uid 10001 (see below) |
| `/etc/graft/post-land-token` | `/run/secrets/graft-post-land-token` in `graft-post-land` | `up.sh` with `GRAFT_POST_LAND=1`; scopes `read`, `attest`, `write` on `graft` and `land` on the checked line (see below) |

`up.sh` writes them from a throwaway root container of the pinned image, so
the runner user needs only the docker group, not sudo. A missing file stops
the container from starting rather than being created as a directory.

The mirror token has the stack's maximum TTL (30 days, scope
`repo:rusttorrent:admin`). `up.sh` re-mints it when it is older than 20 days
(`GRAFT_TOKEN_RENEW_DAYS`), or on `GRAFT_REMINT=1`, and restarts the mirror.

## Followed repositories (trial 3)

graft-mirror follows one repository per process, so each followed GitHub
repository is its own service, with its own config, token, volume and metrics
port:

| Service | Upstream branch → graft line | graft repo | Config | Token | Volume | Port |
|---|---|---|---|---|---|---|
| `graft-mirror` | rustTorrent `main` → `main` | `rusttorrent` | `mirror.toml` | `mirror-token` | `graft-dev-mirror` | 17890 |
| `graft-mirror-fluent2` | fluent2-egui `main` → `main` | `fluent2-egui` | `mirror-fluent2.toml` | `mirror-token-fluent2` | `graft-dev-mirror-fluent2` | 17891 |
| `graft-mirror-vogt` | vogt `rust-core` → `rust-core` | `vogt` | `mirror-vogt.toml` | `mirror-token-vogt` | `graft-dev-mirror-vogt` | 17892 |

vogt follows `rust-core`, not `main` (operator decision): the Rust port
(crate `engine/core`) lives there. The mirror fetches only the branch its
config names, so the initial import is `rust-core`'s history, and the graft
repository `vogt` has the line `rust-core` and no `main`. The line keeps the
branch's name so that `graft-mirror pr-batch <line> --base rust-core` reads
plainly: trial-3 batch PRs for vogt target `rust-core`. A trial line on vogt
forks from `rust-core` (for `trial/create-trial-line.sh`, the import maps
`refs/heads/rust-core`, not `main`).

There is no separate import step. `up.sh` creates every repository
(`admin repo create`, idempotent) and mints each mirror's token before it
starts the mirrors, and a mirror's first `follow` against an empty repository
is the initial import, as it was for `rusttorrent`. So the first deploy after
a repository is added imports it, and a re-run changes nothing. `up.sh`
pre-flights every mirror config, and waits up to `GRAFT_REFS_WAIT` seconds
for each repository's first ref. A repository that still has no ref when `GRAFT_REFS_WAIT` expires fails
the deploy with "no ref after import". That is distinct from the "importing"
log printed while the wait is still running, and from an error returned by
the refs endpoint.

### Onboard another repository

Run this from the graft checkout on the graft-dev VM, in a plain operator
terminal on the graft-dev checkout:

```sh
deploy/onboard-repo.sh OWNER/REPO BASE [LINE]
```

`LINE` defaults to `trial-1`. Use `--dry-run` first to print every planned
file and operational action without changing the checkout, deploying, or
minting credentials. The command adds one row to `mirrors.tsv`, writes the
language-agnostic mirror config, regenerates `compose.onboarded.yml`, and
runs `deploy/up.sh`. The normal deploy path creates the graft repository,
mints its mirror token, starts the service, and waits for that repository's
first imported ref before the command creates the integration line.

A private upstream (one anonymous `git ls-remote` cannot read) gets a
`[github] token_file` pointing at `/run/secrets/github-token`, mounted from
`/etc/graft/github-token` on the host. graft-mirror reads that file itself
and sends it as git's `http.extraHeader`, in the environment of each git
command it runs against upstream (ls-remote, fetch and push), so `follow` and
a separately exec'd `pr-batch` authenticate the same way (WI-2571). The
mirror's entrypoint, `deploy/private-follow.sh`, only warns when the file is
empty. The token is not written into the config, printed, or placed in argv. With an empty token file a private mirror still starts, but its
polls fail and retry until the file is placed. Every mirror, public and private,
also uses that file as its `[github] token_file` so `pr-batch` can push
`graft/batch-*` and open a pull request. The graft-dev runner does not talk
to Infisical. Someone with access places the token once on the VM, outside
the deploy (the value is never put on argv):

```sh
secret get HOMELAB_VOGT_GITHUB_TOKEN --project apps | install -m 0600 /dev/stdin /etc/graft/github-token
```

`up.sh` never reads that file as the runner user. Its root helper
(`as_root`, a throwaway root container) sets it to mode 0600, owned by uid
10001, like the other secrets, so the mirrors can read it. If it is missing
or empty, the deploy still succeeds: it logs one warning, leaves an empty
0600 file for the mount, and starts every mirror without `[github]` (private
mirrors poll without credentials and retry). `pr-batch` then fails with
`pr-batch opens PRs: configure [github]` until the file is placed and the
deploy re-run. The configs the mirrors mount are written by the same helper
to `/etc/graft/mirror-configs`, and the last good deploy records those
mounted copies. The value is never printed or set as a
compose environment value. Place the host file before onboarding a private
repository if you deploy without `up.sh`. Generated mirror configs are mode
0644 so that user can read them.

### Open a batch pull request

From the graft-dev VM, after the line has landed changes to export:

```sh
docker exec graft-dev-mirror-<svc> graft-mirror pr-batch <line> --base <branch> --config /etc/graft/deploy/mirror.toml
```

`<svc>` is the service name without the `graft-` prefix (`mirror` for
rustTorrent, `mirror-fluent2`, `mirror-vogt`, `mirror-rustnzb`,
`mirror-myaiagent`). The command uses `/run/secrets/github-token`, which is
`HOMELAB_VOGT_GITHUB_TOKEN`. It does not take a token on the command line.

Session credentials follow D10 and `trial/mint-agent-tokens.sh`: one
`--kind agent` token per session, minted with `--session`, scopes `read`,
`write`, `drafts` and `land:<line>`. Mint it when the session starts:

```sh
deploy/onboard-repo.sh --mint-session OWNER/REPO ses_… [--work-item WI-N]
```

The token is written to a mode-0600 file and is never printed, passed in
argv, or stored for another session to share. A rerun with the same base
leaves the mirror and line unchanged. A rerun that names a different base
fails instead of silently keeping the registered one. The command prints the
D28 ruleset checklist that must be applied to the GitHub repository before
its first batch is landed.

Check that a mirror has caught up (lag 0) on the VM:

```sh
for p in 17890 17891 17892; do curl -s "http://$(tailscale ip -4):$p/metrics" | grep '^graft_mirror_lag'; done
docker logs --tail 20 graft-dev-mirror-vogt
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
with `GRAFT_POST_LAND=1` in `/etc/graft/deploy.env`. It reads the mirrors'
`github-token` (shared, read-only) and its own `post-land-token`; `up.sh`
refuses to enable it without the first and mints the second. The token
needs `write` on the repository as well as `land` on the line, because a
revert requires write on every path the bad land touched. The procedure,
the measurements, rotation and the switch to `backlog-1` are in
[docs/runbooks/v02-exit.md](../docs/runbooks/v02-exit.md).

## Deploy safety: pre-flight and rollback (WI-2291)

A config can need a newer image than `image.env` pins (after #77,
graft-server refused an unknown `[[lines]]` field and crash-looped). `up.sh`
guards both sides of the swap:

- **Pre-flight.** After pulling the pinned image and before touching the
  running stack, it checks `server.toml` and every mirror config with that image's
  own binaries, in throwaway containers with no network and no secrets.
  graft-server loads its config before dispatching, so
  `graft-server --config server.toml admin` exits 78 on a bad config and 64
  (usage) on a good one. graft-mirror has no check command; `follow` with no
  token mounted gets as far as reading the token file only if its config
  parsed. A failure stops the deploy; the running stack is unchanged.
- **Readiness gate and rollback.** After `compose up --wait` it waits up to
  `GRAFT_READY_WAIT` seconds (default 120) for `/readyz`. If the server or
  the stack doesn't come up, it copies the last good `image.env`,
  `server.toml` and `mirror.toml` back into the checkout, brings that stack
  up, and exits non-zero either way.
- **Last good.** A deploy that passes `/healthz` and `/readyz` records its
  `image.env`, `server.toml` and `mirror.toml` under
  `/etc/graft/last-good/` (root-owned, 0644; no secrets). The first deploy
  with this script has none to roll back to, and says so.

`deploy-dev.yml` fails the run when `up.sh` exits non-zero: an `::error::`
annotation names the cause, and the job summary says the deploy was refused
or rolled back.

## Deploy by hand

```sh
deploy/up.sh
```

On a fresh VM this bootstraps everything: the signing key, the server, the
repositories `rusttorrent`, `graft-dev`, `fluent2-egui` and `vogt`, the mirror
tokens and the mirrors. It then checks `/healthz` and
`GET /v1/repos/<repo>/refs` for each followed repository (authenticated with
that mirror's token, which goes to curl on stdin and is never printed),
waiting up to `GRAFT_REFS_WAIT` seconds (default 600) for the first imports.

The mirror maps git branch `main` to the graft line `main`, so the refs answer
lists `{"kind":"line","name":"main",...}` (graft names, not `refs/heads/main`).
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
