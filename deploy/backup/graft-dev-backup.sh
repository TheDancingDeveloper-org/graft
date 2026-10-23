#!/usr/bin/env bash
# Back up the graft-dev stack (WI-2307): `graft-server admin backup --objects`
# from a throwaway container of the pinned image, into a host directory that
# is not one of the stack's volumes. Safe while the server runs: the metadata
# is a SQLite `VACUUM INTO` snapshot and objects are immutable.
#
# Layout under $GRAFT_BACKUP_DIR (default /var/backups/graft-dev):
#   current/meta.db        the newest metadata snapshot
#   current/objects/       the object tree, copied incrementally
#   meta/meta-<UTC>.db     dated metadata snapshots, the newest
#                          $GRAFT_BACKUP_KEEP (default 14) kept
#   last-success           the time of the last good run (seconds since epoch)
#
# Objects are a superset of what any snapshot references (there is no gc in
# this release), so any dated meta.db plus current/objects restores. The
# signing key and tokens are not backed up: re-create them (deploy/README.md).
#
# With GRAFT_BACKUP_REMOTE set (an rsync destination such as
# host:/srv/backups/graft-dev), the directory is then copied off the VM.
#
# Installed by deploy/backup/install.sh as a systemd timer; run it by hand
# with sudo for an ad hoc backup.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
deploy=$(cd "$here/.." && pwd)
dest=${GRAFT_BACKUP_DIR:-/var/backups/graft-dev}
keep=${GRAFT_BACKUP_KEEP:-14}
remote=${GRAFT_BACKUP_REMOTE:-}

log() { printf 'graft-dev-backup: %s\n' "$*" >&2; }

set -a
# shellcheck source=/dev/null
. "$deploy/image.env"
set +a

case "$dest" in
  /var/lib/docker/*) log "error: $dest is inside docker's data root"; exit 1 ;;
esac
mkdir -p "$dest/current" "$dest/meta"
# One run at a time: two concurrent copies race on the same temp files.
exec 9>"$dest/.lock"
flock -n 9 || { log "error: another backup holds $dest/.lock"; exit 1; }
chown 10001:10001 "$dest/current"

stamp=$(date -u +%Y%m%dT%H%M%SZ)
# The meta volume is mounted read-write because SQLite in WAL mode needs its
# -shm file even to read; the snapshot itself only reads.
docker run --rm --user 10001:10001 --network none --read-only \
  -e GRAFT_SERVER_CONFIG=/etc/graft/deploy/server.toml \
  -v "$deploy/server.toml:/etc/graft/deploy/server.toml:ro" \
  -v graft-dev-meta:/var/lib/graft/meta \
  -v graft-dev-objects:/var/lib/graft/objects:ro \
  -v "$dest/current:/backup" \
  "$GRAFT_IMAGE" graft-server admin backup /backup --objects >&2

cp --reflink=auto "$dest/current/meta.db" "$dest/meta/.meta-$stamp.db.tmp"
mv "$dest/meta/.meta-$stamp.db.tmp" "$dest/meta/meta-$stamp.db"
# Retention: keep the newest $keep dated snapshots.
find "$dest/meta" -maxdepth 1 -name 'meta-*.db' -printf '%f\n' | sort -r \
  | tail -n +"$((keep + 1))" | while read -r old; do rm -f "$dest/meta/$old"; done

if [ -n "$remote" ]; then
  log "copying to $remote"
  rsync -a --delete "$dest/" "$remote/"
fi

date +%s > "$dest/last-success"
log "ok: $dest/meta/meta-$stamp.db ($(find "$dest/meta" -name 'meta-*.db' | wc -l) kept)"
