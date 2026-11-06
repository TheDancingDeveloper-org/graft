# Mirror service cutover (D41 §6 steps 2–4)

Moves graft-dev's thirteen per-repository `graft-mirror follow` containers
onto one `graft-mirror serve`. Design: `docs/notes/mirror-service.md` §6.
Run it on the graft-dev VM, from `/opt/graft`, as an operator. Nothing here
prints a secret: every token and credential goes from its store into a
`0600` file and is read from that file, never from argv.

Rollback is possible until step 7 (the old containers are only scaled to
zero, and the per-repository data layout is unchanged apart from the two
renames). See "Rollback" below.

## 1. Provision `credential_kek`

32 random bytes, root-only, unrelated to the signing key:

```sh
sudo sh -c 'umask 077; head -c 32 /dev/urandom > /etc/graft/kek-k1'
```

Mount it into graft-server and name it in `server.toml`:

```toml
[mirror]
allowed_hosts = ["github.com"]
credential_kek = { id = "k1", file = "/run/secrets/kek-k1" }
```

Store a copy in Infisical (`secret set GRAFT_DEV_CREDENTIAL_KEK --from-file
/etc/graft/kek-k1`): without it, sealed credentials cannot be opened after
a VM rebuild. Redeploy the server through `deploy/up.sh`.

## 2. Mint one `mirror`-kind token

```sh
docker compose --env-file deploy/image.env -f deploy/compose.yml exec -T graft-server \
  graft-server admin token mint --subject graft-mirror --kind mirror \
  --scope 'repo:*:mirror' --ttl 2592000 \
  | sudo sh -c 'umask 077; cat > /etc/graft/.t && [ -s /etc/graft/.t ] && mv /etc/graft/.t /etc/graft/mirror-service-token'
```

## 3. Stage the upstream credentials

One file per repository, named after the graft repository, holding that
repository's upstream PAT (today's `GRAFT_DEV_MIRROR_TOKEN_*` deploy secret,
or the shared `/etc/graft/github-token` where the repository used it):

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
cut -d'|' -f1 deploy/mirrors.tsv | grep -v '^#' \
  | xargs docker compose --env-file deploy/image.env \
      -f deploy/compose.yml -f deploy/compose.onboarded.yml stop
docker ps --format '{{.Names}}' | grep graft-mirror   # must print nothing
```

The old processes do not read the lease, so they must be stopped before the
new service can start (§6 step 7).

## 5. Move (not copy) the per-repository directories

Each old volume holds `<repo>.git` and `<repo>.sqlite`. Move each volume's
contents into `<data>/<repo>/` of the new service's volume, on the same
filesystem so it is a rename. Use `mv`, never `cp`: a copy left behind is a
second directory that a restored old container would run.

```sh
DATA=/var/lib/docker/volumes/graft-mirror-service/_data
sudo mkdir -p "$DATA/vogt"
sudo mv /var/lib/docker/volumes/<vogt-volume>/_data/* "$DATA/vogt/"
# ... one per repository
```

## 6. Run `import-config`

As the operator (the command needs `mirror:bind`). Dry run first; it reads
the server and prints the plan, changing nothing:

```sh
export GRAFT_ENDPOINT=http://127.0.0.1:7878
graft-mirror import-config deploy/mirror.toml deploy/mirror-*.toml \
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

One container, the token from step 2, the data volume from step 5:

```toml
# /etc/graft/mirror-service.toml
server = "http://graft-server:7878"
token_file = "/run/secrets/graft-mirror-token"
data_dir = "/var/lib/graft/mirror"
metrics_addr = "0.0.0.0:9465"
json_logs = true
```

`serve` adopts only marked directories whose marker names that repository.
If a directory is copied to a second service anyway, the repository's lease
lets only one of them run it (§3.6). Check `graft repo mirror status` shows
every row polling, then continue with §6 step 5 (removing the old services)
as its own change.

## Rollback (before §6 step 5)

1. Scale `graft-mirror serve` to zero first.
2. Move each `<data>/<repo>/mirror.git` and `mirror.sqlite` back to the old
   volume as `<repo>.git` and `<repo>.sqlite`.
3. Start the old services from the previous manifest. The rows, leases and
   requests are additive and the old processes ignore them.
