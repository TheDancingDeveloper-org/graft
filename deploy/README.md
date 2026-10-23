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

`up.sh` writes them from a throwaway root container of the pinned image, so
the runner user needs only the docker group, not sudo. A missing file stops
the container from starting rather than being created as a directory.

The mirror token has the stack's maximum TTL (30 days, scope
`repo:rusttorrent:admin`). `up.sh` re-mints it when it is older than 20 days
(`GRAFT_TOKEN_RENEW_DAYS`), or on `GRAFT_REMINT=1`, and restarts the mirror.

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
