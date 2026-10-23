# graft-dev: backups, restore and alerts (WI-2307)

The graft-dev stack (`deploy/`) keeps its state in two docker volumes:
`graft-dev-meta` (SQLite `meta.db`, a few MB) and `graft-dev-objects` (the
object store, about 650 MB in October 2026). `graft-dev-mirror` holds only the
mirror's git cache and is rebuilt from GitHub; it is not backed up. Nor are
the secrets in `/etc/graft`: re-create them as `deploy/README.md` describes
(a new signing key invalidates every token, and `up.sh` mints the mirror's).

## Backups

`deploy/backup/graft-dev-backup.sh` runs `graft-server admin backup <dir>
--objects` in a throwaway container of the pinned image (`--network none`,
uid 10001). It is safe while the server runs: metadata is a SQLite
`VACUUM INTO` snapshot, and objects are immutable and copied incrementally.

| Path (under `/var/backups/graft-dev`) | What |
|---|---|
| `current/meta.db`, `current/objects/` | the newest snapshot and the object tree |
| `meta/meta-<UTC>.db` | dated metadata snapshots; the newest `GRAFT_BACKUP_KEEP` (14) are kept |
| `last-success` | epoch seconds of the last good run |

Any dated `meta.db` restores against `current/objects`, because objects are a
superset of what any snapshot references (there is no gc in this release;
revisit when `admin gc` ships).

Install the timer once, with sudo, from the runner's checkout so it follows
the pinned image in `deploy/image.env`:

```sh
sudo /home/runner/actions-runner/_work/graft/graft/deploy/backup/install.sh
systemctl list-timers graft-dev-backup.timer
sudo systemctl start graft-dev-backup.service   # an ad hoc run
journalctl -u graft-dev-backup.service -n 20
```

It runs every 6 hours (`00/6:17`, randomized by 10 minutes). Settings go in
`/etc/graft/backup.env`: `GRAFT_BACKUP_DIR`, `GRAFT_BACKUP_KEEP`, and
`GRAFT_BACKUP_REMOTE` (an rsync destination; then the directory is copied
off the VM after each run). A lock stops two runs overlapping.

**Gap:** the VM has one disk (`/dev/sda1`, 48 GB), so until
`GRAFT_BACKUP_REMOTE` is set the backup protects against a broken volume or
a bad write, not against losing the VM.

## Restore

Rehearse (never touches the live volumes):

```sh
sudo deploy/backup/restore-rehearsal.sh [/var/backups/graft-dev] [meta-snapshot]
```

It copies the snapshot and `current/objects` to `/var/tmp/graft-restore.*`,
writes a scratch config with a throwaway signing key, and runs
`graft-server admin fsck <repo>` for `rusttorrent` and `graft-dev` in a
container with no network. Exit 0 means every repository is clean.

Real restore, into the live volumes (stops the stack):

```sh
cd deploy
docker compose --env-file image.env -f compose.yml stop
docker run --rm -v graft-dev-meta:/m -v graft-dev-objects:/o \
  -v /var/backups/graft-dev:/b:ro --entrypoint sh "$GRAFT_IMAGE" -c '
    set -e
    rm -f /m/meta.db /m/meta.db-wal /m/meta.db-shm
    cp /b/meta/meta-<UTC>.db /m/meta.db
    cp -a /b/current/objects/. /o/
    chown -R 10001:10001 /m /o'
# fsck against the live volumes, server still stopped:
docker compose --env-file image.env -f compose.yml run --rm --no-deps graft-server \
  graft-server admin fsck rusttorrent
./up.sh
```

(`--user 0:0` on the `docker run` if the image's default user can't write the
volumes; `GRAFT_IMAGE` from `deploy/image.env`.)

## Rehearsal record, 2026-10-23

On graft-dev, from a scratch copy of this branch's `deploy/`, to
`~/scratch/wi-2307-backup` (live stack untouched; both containers stayed
healthy throughout):

| Step | Result |
|---|---|
| first backup | 16,573 objects, 648,172,041 bytes copied, 94 s |
| second backup | 0 copied, 16,573 present, 2 s |
| `restore-rehearsal.sh ~/scratch/wi-2307-backup` | 7 s, exit 0 |
| `fsck rusttorrent` | 311 changesets, 2193 trees, 5053 blobs, 361,980,976 bytes, 2 refs, **0 problems** |
| `fsck graft-dev` | empty, **0 problems** |

An earlier attempt failed with `copy objects: No such file or directory`:
an ssh timeout left a first run going while a second started, and the two
raced on the same temp files. The script now takes a lock.

## Alerts

Rules: `deploy/monitoring/graft-dev-alerts.yml`. Scrape config:
`deploy/monitoring/prometheus-scrape.yml`. Both are applied in the ops repo
(`personal/grafanaloki/prometheus/`) and deployed with the `grafanaloki`
Komodo stack, as in the host-monitoring skill.

| Alert | Fires when |
|---|---|
| `GraftDevServerDown` | blackbox `/readyz` probe fails for 5 m |
| `GraftDevMetricsDown` | server or mirror `/metrics` unscraped for 10 m |
| `GraftDevDiskLow` / `Critical` | `graft_disk_free_bytes / total` < 15 % for 15 m / < 8 % for 5 m (writes stop at 5 %) |
| `GraftDevMirrorLag` | `graft_mirror_lag_seconds` > 30 min for 15 m |

When one fires: `docker ps` and `docker logs --tail 100 graft-dev-server` (or
`graft-dev-mirror`) on the VM; for disk, `docker system df` and the backup
directory; for mirror lag, the mirror log (GitHub reachability, an expired
token: `GRAFT_REMINT=1 deploy/up.sh`).
