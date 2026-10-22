# Local dev stack

The dev stack runs graft-server, graft-node, graft-mirror and the `graft` CLI
under docker compose on one host, so server and client work can be iterated
without a deploy cycle. G1.17 then promotes this compose file to a Komodo
stack.

Today the binaries are stubs. Each prints `not implemented yet` and exits, and
the stack retries it a few times and stops. The stack is written so the real
binaries work unchanged once they land.

## Files

| Path | What it is |
| --- | --- |
| `docker/Dockerfile` | one multi-stage image: cargo-chef dependency cache, rust builder, debian trixie-slim runtime with git and ca-certificates, non-root user `graft` (uid 10001) |
| `.dockerignore` | keeps `target/`, `.git` and local agent state out of the build context |
| `compose.yaml` | the services, profiles and named volumes |
| `docker/dev/server.toml` | server config mounted at `/etc/graft/server.toml` |
| `docker/dev/mirror/` | fixture and rustTorrent mirror configs, and the fixture entrypoint |
| `docker/dev/prometheus/`, `docker/dev/grafana/` | scrape config and Grafana datasource |
| `scripts/dev/` | `up.sh`, `down.sh`, `seed.sh`, `smoke.sh`, `crash.sh` |

Images are `graft-dev:local`. Named volumes are `graft-dev_*`.

## Where the daemon must be

Bind mounts (`docker/dev/`, and in dev mode `target/debug/`) are resolved on
the Docker daemon's filesystem. `scripts/dev/lib.sh` computes
`GRAFT_HOST_ROOT`, the checkout's path on that filesystem, and compose uses it
for every bind source. Locally it is the checkout itself.

With a remote daemon, `lib.sh` maps the checkout's mount point to the subpath
`findmnt` reports, then checks the mapping by reading `README.md` through a
read-only `/` mount on the daemon. The check reads only; it never bind-mounts
the missing path. A mismatch stops `up.sh` with a message.

**Current finding (not resolved):** on the `vogt-prod` checkout, `findmnt`
reports `/home/sprooty/Working` as `/dev/nvme0n1p1[/vogt/prod/workspace]`, a
local disk on vogt-prod. The daemon behind `/var/run/docker.sock` is Node B
(`winrarhost`), and `/vogt/prod/workspace` does not exist there. So the mapping
fails and `up.sh` refuses. Earlier bind attempts left empty root-owned
directories under `/vogt` on Node B. They need removing by someone with access.

Options: a local daemon on vogt-prod, a host that has a local daemon, or
copying the checkout to Node B and mapping that copy. `docker compose watch`
can sync binaries over the API, but its config bind mounts still need the
checkout on the daemon's host, so it doesn't avoid this.

## Profiles

| Profile | Services | Use |
| --- | --- | --- |
| (none, default) | `graft-server`, `graft-node` | the image binaries; plain `docker compose ps` and `logs` show them |
| `dev` | `graft-server-dev`, `graft-node-dev`, `graft-mirror-dev` | fast loop: host `target/debug` binaries bind-mounted over the image copies |
| `mirror` | `graft-mirror` | follows the local fixture repository |
| `rusttorrent` | `graft-mirror-rusttorrent` | clones `TheDancingDeveloper-org/rustTorrent` |
| `obs` | `prometheus`, `grafana` | scrapes `/metrics` on the loopback ports below |
| `tools` | `client` | the `graft` CLI in a toolbox container |

`up.sh --dev` starts the `dev` variants and scales the default server and node
to 0, so only one of each runs at a time. The variants keep the image
services' names as network aliases (`graft-server`, `graft-node`,
`graft-mirror`), so the client, the mirror config and Prometheus resolve the
same names in both modes. Don't start a `dev` variant with its default twin
by hand: they publish the same port.

## Loop

```sh
scripts/dev/up.sh                    # build graft-dev:local if missing; start server and node
docker compose run --rm client graft version

# after changing Rust code:
cargo build -p graft-server -p graft-node -p graft-mirror
scripts/dev/up.sh --dev              # host binaries; the image server and node are scaled to 0
docker compose --profile dev restart graft-server-dev   # pick up a rebuilt binary
```

Run `docker compose build` after changing the Dockerfile or the workspace
dependencies. `up.sh` builds only when `graft-dev:local` is missing; it does
not rebuild on every run. `up.sh --dev` sets the mode bits of the three host
binaries to world-readable and executable, because the container runs as
uid 10001.

### Host binaries and glibc

The dev profile runs binaries built on the host inside `debian:trixie-slim`,
which ships glibc 2.41. A binary built against a newer glibc will not start.
This checkout's host has glibc 2.43, so a `GLIBC_2.4x not found` error means
the dev loop has to build inside the builder image, or use image mode.

## Ports

Host ports bind to `127.0.0.1` only, and default to values that are free on
the dev host. The usual ones (7878, 9090, 3000) are taken by other services
there:

| Service | Host port | Variable |
| --- | --- | --- |
| graft-server (and `graft-server-dev`) | 17878 | `GRAFT_DEV_SERVER_PORT` |
| prometheus | 19090 | `GRAFT_DEV_PROMETHEUS_PORT` |
| grafana | 13100 | `GRAFT_DEV_GRAFANA_PORT` |

Node, mirror and client publish nothing. The node serves a Unix socket at
`/run/graft/node.sock` in the shared `graft-dev_graft-dev-run` volume, and the
client reads it there.

## Scripts

- `up.sh [--dev] [--mirror] [--rusttorrent] [--obs]`. Starts the stack with
  `--remove-orphans`, so switching modes removes the other mode's containers.
- `down.sh [--wipe]`. Stops everything in every profile. Keeps the named
  volumes unless `--wipe`, which also deletes the server's objects and
  metadata, the node cache, and the mirror fixtures.
- `seed.sh [repo]`. Creates the repository with `graft-server admin repo create`
  and imports the fixture with `graft-mirror import`. Run once, with the real
  binaries. Exits 2 with the stub's message until then.
- `smoke.sh [repo]`. Runs `graft clone`, `log`, `diff` and `blame` in the
  client toolbox. Exits 2 with `not implemented yet` while stubbed, 1 on any
  real failure, 0 when all four pass.
- `crash.sh`. Sends SIGKILL to the running server while `smoke.sh` runs,
  waits for the restart policy to bring it back (up to 60 s), and runs
  `smoke.sh` again. Exits 2 while the server is stubbed.

## Restart policy

Every runtime service has `restart: on-failure:3`. `docker inspect` shows
`RestartCount=3` after a stub has run out of retries. A stub exits 2 once, and
Docker retries it three times at most, then leaves it exited. `docker compose
ps` shows the final state. The obs services use `unless-stopped`.

## Secrets

- Services listen on `0.0.0.0` inside the container only, because the
  container's network is private. Published ports stay on loopback.
- No secrets live in these files. The mirror clones GitHub anonymously. Grafana
  allows anonymous editor access, so keep it on loopback.

## Verification status

Checked before the guard was added, on the daemon host (Node B):

- `docker compose config -q`, and with `--profile dev`, `mirror`,
  `rusttorrent`, `obs` and `tools`: valid.
- `docker compose build`: succeeds (the image runs as uid 10001 on trixie).
- `up.sh` (with the earlier override): both stubs log their `not implemented
  yet` line, retry three times and stop (`RestartCount=3`). That run mounted
  `docker/dev/server.toml` on Node B, which created an empty directory there.
- `up.sh --dev`: stopped by the mapping guard on that daemon (see above), so
  the dev loop, its glibc behaviour, `develop.watch` and `crash.sh` are
  unverified.
  `smoke.sh`, `seed.sh` and `mirror`, `obs` and `tools` are not yet run against
  live binaries.

## Volumes

| Volume | Holds |
| --- | --- |
| `graft-dev_graft-dev-server-objects` | object store (`/var/lib/graft/objects`) |
| `graft-dev_graft-dev-server-meta` | SQLite metadata (`/var/lib/graft/meta/meta.db`) |
| `graft-dev_graft-dev-run` | node socket, shared with the client |
| `graft-dev_graft-dev-node-cache` | node object cache |
| `graft-dev_graft-dev-mirror-fixture` | fixture git repository, mirror state |
| `graft-dev_graft-dev-mirror-rusttorrent` | rustTorrent mirror state |
| `graft-dev_graft-dev-client-work` | client working copies (`/work`) |
| `graft-dev_graft-dev-prometheus`, `graft-dev_graft-dev-grafana` | obs data |

## Open items

These need decisions from the crates that own the behaviour:

- **Config keys.** `server.toml`, the mirror TOML files and the node settings
  use the field names from the stubs' defaults (`listen`, `objects_dir`,
  `meta_db`, `min_free_fraction`, the node's socket and cache paths). The
  metrics key and port (`metrics = "0.0.0.0:9464"`) are a dev convention.
  G1.12 (server), G2.3 (node) and G1.14 (mirror) decide the real keys.
- **Dev-mode daemon.** The dev loop needs the checkout on the daemon's host
  (see above): a local daemon here, or a host that has one.
- **Node upstream.** The node stub has no way to set its upstream server.
  Once the real parser lands, add it to the node command.
- **Healthcheck.** Add `healthcheck` to `graft-server` once `/healthz` exists.
- **Client endpoint.** The client finds the node socket by itself. Set
  `GRAFT_ENDPOINT` only if the CLI cannot find it.
- **Komodo.** G1.17 maps the default services onto a Komodo stack. Mutations
  for `vogt-dev` only; anything under a `*prod*` name needs an explicit opt-in.
