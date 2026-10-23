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
| `up.sh` | idempotent deploy: bootstrap secrets, pull, `up -d --wait`, health check |
| `attester/Dockerfile` | the trial attester's image (profile `attester`, off by default; see below) |

The workflow is `.github/workflows/deploy-dev.yml`: on a push to `main` that
touches `deploy/**`, and on `workflow_dispatch`, it runs `deploy/up.sh` on the
runner labelled `[self-hosted, graft-dev]`. It is not a required check. The image is public on GHCR, so the job does
not log in to a registry.

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

## Secrets

None in git, in the workflow, or in the compose environment. They are host
files under `/etc/graft` (override with `GRAFT_SECRETS_DIR`), mode 0600, owned
by uid 10001, mounted read-only:

| File | Mounted at | Created by |
|---|---|---|
| `/etc/graft/signing.key` | `/run/secrets/signing.key` | `up.sh`, first run (32 random bytes) |
| `/etc/graft/mirror-token` | `/run/secrets/graft-mirror-token` | `up.sh`, minted by the running server |
| `/etc/graft/attester-token` | `/run/secrets/graft-attester-token` | `up.sh` with `GRAFT_ATTESTER=1`; owned by root, not uid 10001 (see below) |

`up.sh` writes them from a throwaway root container of the pinned image, so
the runner user needs only the docker group, not sudo. A missing file stops
the container from starting rather than being created as a directory.

The mirror token has the stack's maximum TTL (30 days, scope
`repo:rusttorrent:admin`). `up.sh` re-mints it when it is older than 20 days
(`GRAFT_TOKEN_RENEW_DAYS`), or on `GRAFT_REMINT=1`, and restarts the mirror.

## Trial attester (WI-2276)

The contained trial (WI-2270) gets its `ci/build` and `ci/test`
attestations from a small attester on this VM, not from GitHub. It is the
`graft-attester` service (crate `crates/graft-attester`), in the compose
profile `attester`, and is **off by default**. Turn it on in
`/etc/graft/deploy.env`:

```sh
echo "GRAFT_ATTESTER=1" | sudo tee -a /etc/graft/deploy.env
deploy/up.sh
```

With it on, `up.sh` also:

- creates the repository `rusttorrent-trial` (`GRAFT_ATTESTER_REPO`);
- mints `/etc/graft/attester-token`, kind `service`, scopes
  `repo:rusttorrent-trial:read` and `repo:rusttorrent-trial:attest`, with the
  stack's TTL and the same renewal as the mirror token (`GRAFT_REMINT=1`
  forces it). `attest` grants `Attest` only, so `read` is needed to watch
  the journal and check drafts out. It needs a server that knows the
  `attest` scope (WI-2272);
- builds the image from this checkout (`deploy/attester/Dockerfile`) and
  starts it.

What it does:

- It reads the journal of `rusttorrent-trial` (`Watch`, long-poll) for
  `ChangesetCreated` entries of drafts held by a workspace. It keeps the
  newest draft per workspace, since a land needs the stack's head
  attested. The queue holds at most 16 drafts. When it is full the attester
  stops reading, and the journal holds the rest.
- It checks one draft at a time: it confirms the changeset is still a
  draft, checks its tree out into `/var/lib/graft-attester/work/<draft>`,
  runs `cargo build --locked` (`ci/build`), then `cargo test --locked`
  (`ci/test`), and posts `Attest` for each, passed or failed. A check that
  fails makes the later ones fail without running. Each check has a
  timeout (`GRAFT_ATTESTER_TIMEOUT_S`, default 2700 s). At the timeout, or
  when the check ends, its whole process group is killed.
- The attested URL is the log's path,
  `file:///var/lib/graft-attester/logs/<draft>/ci-build.log`, in the
  `graft-dev-attester` volume. Each log is capped at 8 MiB and kept for 14
  days. To read one:
  `docker exec graft-dev-attester cat /var/lib/graft-attester/logs/<draft>/ci-test.log`.
- The read position is kept in `/var/lib/graft-attester/state/lsn`. A
  restart re-runs at most the drafts that were still queued.

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
- The checks have network access: `cargo build --locked` fetches crates.

Turning it off: remove `GRAFT_ATTESTER=1`, then
`docker compose --env-file deploy/image.env -f deploy/compose.yml --profile attester rm -sf graft-attester`.
A push that changes only `crates/graft-attester` does not redeploy (the
workflow watches `deploy/**`). Run `deploy/up.sh` or the workflow by hand.

## Deploy by hand

```sh
deploy/up.sh
```

On a fresh VM this bootstraps everything: the signing key, the server, the
repositories `rusttorrent` and `graft-dev`, the mirror token and the mirror. It then checks
`/healthz` and `GET /v1/repos/rusttorrent/refs` (authenticated with the mirror
token, which goes to curl on stdin and is never printed), waiting up to
`GRAFT_REFS_WAIT` seconds (default 600) for the first import.

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
