# Mirror service cutover and retirement (D41 §6 steps 2–5)

Moves graft-dev's thirteen per-repository `graft-mirror follow` containers
onto one `graft-mirror serve`. Design: `docs/notes/mirror-service.md` §6.
Run it on the graft-dev VM, from `/opt/graft`, as an operator. Nothing here
prints a secret: every token and credential goes from its store into a
`0600` file and is read from that file, never from argv.

Steps 1–6 prepare the server and move the data while the old containers are
stopped; step 7 deploys the service; step 8 retires the old files and secrets
and is the point of no return. Rollback is possible until step 8: the old
containers are only stopped, the per-repository data layout is unchanged
apart from the two renames, and `deploy/up.sh` backs up the metadata database
before the new server migrates it. See "Rollback" below.

**Two checkouts.** The retirement change removes `mirrors.tsv`,
`mirror-*.toml`, `compose.onboarded.yml` and the per-repository services from
`deploy/`. Steps 1–6 need those files, so run them from the checkout *before*
the retirement commit (`RETIRE` is its sha; keep the old tree with
`git worktree add /opt/graft-old $RETIRE^`, and use `/opt/graft-old` where
these steps say `deploy/`). Step 1's server and compose edits are already in
the retirement commit; in the old tree, make them by hand. Step 7 runs
`deploy/up.sh` from the new tree.

## 1. Provision `credential_kek`

32 random bytes, root-only, unrelated to the signing key:

```sh
sudo sh -c 'umask 077; head -c 32 /dev/urandom > /etc/graft/kek-k1'
```

In the old tree, mount it into graft-server (a read-only bind of
`/etc/graft/kek-k1` at `/run/secrets/kek-k1`, as `signing.key` is) and name it
in `server.toml` (the retirement commit carries both):

```toml
[mirror]
allowed_hosts = ["github.com"]
credential_kek = { id = "k1", file = "/run/secrets/kek-k1" }
```

Store a copy in Infisical (`secret set GRAFT_DEV_CREDENTIAL_KEK --from-file
/etc/graft/kek-k1`): without it, sealed credentials cannot be opened after
a VM rebuild. Redeploy the server through `deploy/up.sh`.

## 2. Mint one `mirror`-kind token (optional now)

Step 7's `up.sh` mints `/etc/graft/mirror-service-token` itself when it is
missing. Mint it here by hand only to have the token before step 7; `up.sh`
then leaves it until it is 20 days old. Same command either way:

```sh
docker compose --env-file deploy/image.env -f deploy/compose.yml exec -T graft-server \
  graft-server admin token mint --subject graft-mirror --kind mirror \
  --scope 'repo:*:mirror' --ttl 2592000 \
  | sudo sh -c 'umask 077; cat > /etc/graft/.t && [ -s /etc/graft/.t ] && mv /etc/graft/.t /etc/graft/mirror-service-token'
```

## 3. Stage the upstream credentials

One file per repository, named after the graft repository, holding that
repository's upstream PAT (today's `GRAFT_DEV_MIRROR_TOKEN_*` deploy secret,
or the shared `/etc/graft/github-token` where the repository used it). These
secrets stay where they are until step 8:

```sh
sudo install -d -m 0700 /run/graft-cutover/creds
secret get GRAFT_DEV_MIRROR_TOKEN_VOGT \
  | sudo sh -c 'umask 077; cat > /run/graft-cutover/creds/vogt'
# ... one per repository in mirrors.tsv
```

`/run` is tmpfs: the files go with the next reboot. Delete them after step 6.

## 4. Scale the old mirrors to zero

Stop every per-repository `graft-mirror*` service (the names are column 1 of
`mirrors.tsv`) and confirm none is running:

```sh
cd /opt/graft-old
cut -d'|' -f1 deploy/mirrors.tsv | grep -v '^#' \
  | xargs docker compose --env-file deploy/image.env \
      -f deploy/compose.yml -f deploy/compose.onboarded.yml stop
docker ps --format '{{.Names}}' | grep graft-mirror   # must print nothing
```

The old processes do not read the lease, so they must be stopped before the
new service can start (§6 step 7).

## 5. Move (not copy) the per-repository directories

Each old volume holds `<repo>.git` and `<repo>.sqlite`. The service's volume
is the compose volume `graft-dev-mirror`, which is also `rusttorrent`'s old
volume: move that one's contents down into `<data>/rusttorrent/` in place, and
every other volume's into `<data>/<repo>/` (the volumes are on one
filesystem, so each is a rename). Use `mv`, never `cp`: a copy left behind is a
second directory that a restored old container would run.

```sh
DATA=/var/lib/docker/volumes/graft-dev-mirror/_data
# rusttorrent: its files are already in $DATA; move them into a subdirectory
sudo mkdir "$DATA/rusttorrent"
sudo sh -c 'mv "$1"/rusttorrent.git "$1"/rusttorrent.sqlite* "$1"/rusttorrent/' _ "$DATA"
# every other repository: its old volume's files into $DATA/<repo>/
sudo mkdir -p "$DATA/vogt"
sudo mv /var/lib/docker/volumes/graft-dev-mirror-vogt/_data/* "$DATA/vogt/"
# ... one per repository (volume names: docker volume ls | grep graft-dev-mirror)
```

## 6. Run `import-config`

As the operator (the command needs `mirror:bind`). Dry run first; it reads
the server and prints the plan, changing nothing:

```sh
export GRAFT_ENDPOINT=http://127.0.0.1:7878
graft-mirror import-config /opt/graft-old/deploy/mirror.toml /opt/graft-old/deploy/mirror-*.toml \
  --data-dir "$DATA" --credential-dir /run/graft-cutover/creds \
  --token-file /etc/graft/sprooty.token --dry-run
```

Each line reads `<repo> <upstream>: row: …; credential: …; dir: …`. Expect
`would create`, `would seed from /run/graft-cutover/creds/<repo>` and
`would mark` for all thirteen. Then run it without `--dry-run`. It:

- creates each `repo_mirrors` row from its config;
- seals each row's credential from `<credential-dir>/<repo>`;
- renames `<repo>.git` / `<repo>.sqlite` to `mirror.git` / `mirror.sqlite`
  and writes the migration marker, which names the repository.

It is idempotent: a second run prints `row: unchanged; credential: already
set; dir: marked` everywhere. A `row: DIFFERS (…)` line means a row already
exists with other settings; it is left alone, along with its credential and
directory, and the command exits non-zero. Settle it with `graft repo mirror
set`, then run again. `dir: MISSING` means step 5 skipped that repository;
`dir: REFUSED` means its marker names another repository (a misplaced
copy). Fix those before step 7.

Then remove the staged credentials: `sudo rm -rf /run/graft-cutover`.

## 7. Deploy one `graft-mirror serve`

From the new tree (the retirement commit), as for any deploy:

```sh
cd /opt/graft && git checkout $RETIRE   # or the branch that carries it
deploy/up.sh
```

`up.sh` checks that `kek-k1` exists, pre-flights `server.toml` and
`mirror-service.toml`, mints `mirror-service-token` if missing, and starts the
single `graft-mirror` service (`graft-mirror serve --config
/etc/graft/deploy/mirror-service.toml`) with `--wait`. `--remove-orphans`
removes the stopped per-repository containers. `serve` adopts only marked
directories whose marker names that repository; if a directory is copied to a
second service anyway, the repository's lease lets only one of them run it
(§3.6).

Check that `graft repo mirror status` shows every row polling and
`curl -s localhost:17890/metrics | grep '^graft_mirror_lag'` has a series for
each of the thirteen repositories. Accept: one serve container follows all
thirteen, and no per-repository mirror container is running
(`docker ps --format '{{.Names}}' | grep graft-dev-mirror` prints only
`graft-dev-mirror`).

## 8. Retire the per-repository files and secrets (point of no return)

Only after step 7 has run clean for a while (at least a day of polls with no
`GraftMirrorStalled`, `GraftMirrorCredentialError` or `GraftMirrorLeaseLost`
alert). This step makes the old layout unrecoverable:

1. Remove the old volumes, now empty: `docker volume rm` for each
   `graft-dev-mirror-<repo>` (not `graft-dev-mirror`).
2. Remove the host files `/etc/graft/mirror-token`, `mirror-token-*` and
   `mirror-configs/`, and `/opt/graft-old`.
3. Delete the `GRAFT_DEV_MIRROR_TOKEN_*` secrets from Infisical, and the ops
   repo's mirror rendering (`gen.py`) in its own PR. `/etc/graft/github-token`
   goes with the last consumer of it.
4. Scrape and alert config: apply `deploy/monitoring/prometheus-scrape.yml`
   (job `graft-mirror`, one target) in the ops repo's Prometheus.

From here, onboarding is `graft repo mirror add` (deploy/README.md).

## Rollback (before step 8 only)

The new server migrates the metadata database (schema v8 to v12) the first
time it opens it, at step 1, and an older build refuses a database newer than
it knows. So rolling back means restoring the database as it was before that
migration, together with the old image: there are no down-migrations, and the
old server does not start on the migrated database.

`deploy/up.sh` takes that backup itself. Whenever it deploys a server image
other than the one running, it stops graft-server first and copies `meta.db`
and its WAL files to `pre-deploy/` on the `graft-dev-meta` volume (with the
image that wrote them in `pre-deploy/image`). If the new stack does not come
up, its `rollback()` restores that copy before it starts the last good image.
Before step 1, confirm `pre-deploy/` exists after the deploy:
`docker run --rm -v graft-dev-meta:/m alpine ls -l /m/pre-deploy`. Keep a
copy of it off the volume until step 8 is done, since the next image change
replaces it.

Restoring the backup loses every write made since step 1: rows, leases,
requests, and any lands, so roll back before the service has followed
anything you need to keep, or re-import those changes after.

Order matters: the old processes do not read the lease, so the service must be
gone before they start (§6 step 7).

1. Scale `graft-mirror serve` to zero first:
   `docker compose --env-file deploy/image.env -f deploy/compose.yml stop graft-mirror`
   (and `rm` it). Confirm `docker ps | grep graft-dev-mirror` prints nothing.
2. Stop graft-server, then restore the pre-migration database:
   ```sh
   docker compose --env-file deploy/image.env -f deploy/compose.yml stop graft-server
   docker run --rm -v graft-dev-meta:/m alpine sh -c 'set -e; cd /m
     test -s pre-deploy/meta.db
     rm -f meta.db meta.db-wal meta.db-shm
     for f in meta.db meta.db-wal meta.db-shm; do
       [ -e pre-deploy/$f ] && cp -p pre-deploy/$f .; done'
   ```
3. Move each `<data>/<repo>/mirror.git` and `mirror.sqlite` back to the old
   volume as `<repo>.git` and `<repo>.sqlite`, and `rusttorrent`'s back up to
   the root of `graft-dev-mirror`.
4. Restore the old deployment, image and token secrets: check out `$RETIRE^`
   (the old tree), set its `deploy/image.env` to the image recorded in
   `pre-deploy/image` (the one that wrote the backup), and run its
   `deploy/up.sh`. Its per-repository services come back. The
   `mirror-token*` files and the `GRAFT_DEV_MIRROR_TOKEN_*` secrets are still
   in place because step 8 has not run.

After step 8, rolling back means re-creating the volumes, tokens and configs
from history and Infisical as well; do not plan on it.
