# Local dev stack

The dev stack runs graft-server, graft-node, graft-mirror and the `graft` CLI
under docker compose, so server and client work can be iterated without a
deploy cycle. G1.17 then promotes this compose file to a Komodo stack.

Today the binaries are stubs. Each prints `not implemented yet` and exits, and
the stack retries it a few times and stops. The stack is written so the real
binaries work unchanged once they land.

## Design: no host paths

The stack works with a Docker daemon on any host, including one reached over a
socket on another machine. It never bind-mounts a path from the checkout:

- **Build mode.** `docker compose build` streams this repository as the build
  context over the API. Configs (`docker/dev/server.toml`, the mirror configs
  and the fixture entrypoint) are baked into the image, so changing one means
  a rebuild. The obs images (`docker/dev/prometheus`, `docker/dev/grafana`) are
  built the same way.
- **Fast loop.** `scripts/dev/reload.sh <service>...` copies the host binary
  in with `docker compose cp` (the API, so no host path is needed) and restarts
  the service. This is the loop to use with a remote daemon. Each graft service
  also has a `develop.watch` block (see below):
  `action: sync+restart` from `./target/debug/<bin>` to
  `/opt/graft-dev/bin/<bin>`. `docker compose watch` would do the same on
  change. It did not sync in this environment, and it honours `.dockerignore`,
  which excludes `target/`; it may not work with a remote daemon. Use
  `reload.sh`. The target is `/opt/graft-dev/bin`, not `/usr/local/bin`: the
  `graft` user cannot write the root-owned `/usr/local/bin`, and the sync
  needs to replace files there. `/opt/graft-dev/bin` comes first on `PATH`;
  with no dev copy, the image binary in `/usr/local/bin` runs.

## Files

| Path | What it is |
| --- | --- |
| `docker/Dockerfile` | one multi-stage image: cargo-chef dependency cache, rust builder, debian trixie-slim runtime with git and ca-certificates, non-root user `graft` (uid 10001); bakes in the dev configs |
| `.dockerignore` | keeps `target/`, `.git` and local agent state out of the build context |
| `compose.yaml` | the services, profiles, watch blocks and named volumes |
| `docker/dev/server.toml` | server config, baked in at `/etc/graft/server.toml` |
| `docker/dev/mirror/` | fixture and rustTorrent mirror configs and the fixture entrypoint, baked in at `/etc/graft-dev/mirror/` |
| `docker/dev/prometheus/`, `docker/dev/grafana/` | obs images: scrape config and datasource provisioning |
| `scripts/dev/` | `up.sh`, `down.sh`, `seed.sh`, `smoke.sh`, `crash.sh` |

Images are `graft-dev:local`, `graft-dev-prometheus:local` and
`graft-dev-grafana:local`. Named volumes are `graft-dev_*`.

## Profiles

| Profile | Services | Use |
| --- | --- | --- |
| (none, default) | `graft-server`, `graft-node` | the image binaries; plain `docker compose ps` and `logs` show them |
| `dev` | `graft-server-dev`, `graft-node-dev`, `graft-mirror-dev` | the fast loop: `docker compose --profile dev watch` |
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
scripts/dev/up.sh                                  # build if missing; start the default server and node
docker compose run --rm client graft version

# after changing Rust code:
cargo build -p graft-server -p graft-node -p graft-mirror
scripts/dev/reload.sh graft-server graft-node      # docker cp the binaries in, then restart each service
```

`reload.sh` takes service names and accepts the `-dev` variants too. It checks
that `target/debug/<bin>` exists, makes it world-executable (the container runs
as uid 10001), copies it to `/opt/graft-dev/bin/<bin>` (graft-owned, first on
`PATH`), and restarts the service. With no dev copy, the image binary in
`/usr/local/bin` runs.

`docker compose --profile dev watch` is the alternative for a local daemon. It
is unverified here (see Verification status).

Run `docker compose build` after changing the Dockerfile, the dev configs or
the workspace dependencies. `up.sh` builds only when an image is missing.
`up.sh --dev` starts the dev variants from the image; run `reload.sh` for the
`-dev` service names to load host binaries into them.

### Host binaries and glibc

The fast loop runs binaries built on the host inside `debian:trixie-slim`,
which ships glibc 2.41. A binary built against a newer glibc will not start.
This checkout's host has glibc 2.43, so a `GLIBC_2.4x not found` error means
the loop has to build inside the builder image, or use the image binaries.

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

- `reload.sh <service>...`. Copies `target/debug/<bin>` into each service's
  container at `/opt/graft-dev/bin/<bin>` with `docker compose cp`, then
  restarts it. Works with a remote daemon.
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
`RestartCount=3` after a stub has run out of retries. The obs services use
`unless-stopped`.

## Secrets

- Services listen on `0.0.0.0` inside the container only, because the
  container's network is private. Published ports stay on loopback.
- No secrets live in these files. The mirror clones GitHub anonymously. Grafana
  allows anonymous editor access, so keep it on loopback.

## Verification status

- `docker compose config -q`, default and with `--profile` `dev`, `mirror`,
  `rusttorrent`, `obs` and `tools`: valid, and no service has a bind mount.
- `docker compose build`: succeeds (rebuilt after the change to the image).
- **Reload cycle: demonstrated.** A stub with a locally changed message (not
  committed) was built on the host and reloaded with `reload.sh graft-server`.
  The container log then showed the new message, and the restart count went
  to 3 with the new binary. The edit was reverted and the host binary rebuilt.
- `up.sh --dev`: the dev variants start from the image.
- **Watch cycle: NOT demonstrated (`develop.watch`).** `docker compose watch` recorded no sync
  for any of these, even though the kernel delivers inotify events for the same
  directories (checked with a raw inotify probe): the real `target/debug` binary
  with the committed `.dockerignore` (which excludes `target`), with a trimmed
  `.dockerignore`, a probe path outside `target`, and a directory sync. A
  probe path synced once earlier in this session, so the cause is not settled.
  `.dockerignore` is the first suspect, since compose watch may skip
  dockerignored paths. A fix needs a decision: either let `target/debug/graft-*`
  into the context (the context then carries the three binaries only), or
  stage the binaries outside `target/`.
- `smoke.sh`, `seed.sh`, `crash.sh`, and the `mirror`, `obs` and `tools`
  profiles are not yet run against live binaries.

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
- **Node upstream.** The node stub has no way to set its upstream server.
  Once the real parser lands, add it to the node command.
- **Healthcheck.** Add `healthcheck` to `graft-server` once `/healthz` exists.
- **Client endpoint.** The client finds the node socket by itself. Set
  `GRAFT_ENDPOINT` only if the CLI cannot find it.
- **Komodo.** G1.17 maps the default services onto a Komodo stack. Mutations
  for `vogt-dev` only; anything under a `*prod*` name needs an explicit opt-in.
