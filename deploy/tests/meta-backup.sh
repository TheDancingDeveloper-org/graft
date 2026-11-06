#!/usr/bin/env bash
# up.sh's backup_meta, restore_meta and rollback: before a new server image
# opens the metadata database (and may migrate it), the running server is
# stopped and meta.db with its WAL files is copied to pre-deploy; a failed
# deploy restores that copy before the last good image starts, so the old
# build never meets a schema newer than it knows. A redeploy of the running
# image, or a fresh host, takes no backup and restores nothing.
#
# The real up.sh functions run here. docker, compose and the root shells are
# replaced: meta_root maps /m to a temp directory.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

log() { printf 'up.sh: %s\n' "$*" >&2; }
die() { printf 'up.sh: %s\n' "$*" >&2; exit 99; }
meta_root() {
  local snippet=${1//cd \/m/cd $meta}
  sh -c "$snippet"
}
docker() {
  # docker inspect -f ... graft-dev-server: the running image, if any.
  [ "$1" = inspect ] || fail "unexpected docker $*"
  [ -n "${RUNNING:-}" ] || return 1
  printf '%s\n' "$RUNNING"
}
compose() { printf '%s\n' "$*" >>"$tmp/compose"; }
as_root() { sh -c "${1//\/s\//$secrets/}"; }
have_last_good() { return 0; }
config_hash() { :; }
wait_ready() { return 0; }
for f in backup_meta restore_meta rollback; do
  body=$(sed -n "/^$f()/,/^}/p" "$root/deploy/up.sh")
  [ -n "$body" ] || fail "up.sh has no $f"
  eval "$body"
done
grep -q '^backup_meta$' "$root/deploy/up.sh" || fail "up.sh never calls backup_meta"
[ "$(grep -n '^backup_meta$' "$root/deploy/up.sh" | cut -d: -f1)" -lt \
  "$(grep -n '^compose up -d --wait graft-server' "$root/deploy/up.sh" | cut -d: -f1)" ] \
  || fail "backup_meta must run before the new server starts"

new_host() {
  meta=$tmp/meta-$1
  secrets=$tmp/secrets-$1
  here=$tmp/here-$1
  mkdir -p "$meta" "$secrets/last-good" "$here"
  printf 'GRAFT_IMAGE=old\n' >"$secrets/last-good/image.env"
  printf 'old-config\n' >"$secrets/last-good/server.toml"
  : >"$tmp/compose"
  meta_backed_up=0
  last_good=last-good
}

# --- a new image: backed up before it starts, restored on rollback ----------
new_host upgrade
printf 'v8-schema\n' >"$meta/meta.db"
printf 'v8-wal\n' >"$meta/meta.db-wal"
RUNNING=old GRAFT_IMAGE=new
backup_meta
[ "$meta_backed_up" = 1 ] || fail "upgrade: no backup recorded"
grep -q '^stop graft-server$' "$tmp/compose" || fail "upgrade: the server was not stopped first"
[ "$(cat "$meta/pre-deploy/meta.db")" = v8-schema ] || fail "upgrade: meta.db not copied"
[ "$(cat "$meta/pre-deploy/meta.db-wal")" = v8-wal ] || fail "upgrade: WAL not copied"
[ "$(cat "$meta/pre-deploy/image")" = old ] || fail "upgrade: image not recorded"
# The new server migrates the database, then fails its health check.
printf 'v12-schema\n' >"$meta/meta.db"
rm -f "$meta/meta.db-wal"
printf 'v12-shm\n' >"$meta/meta.db-shm"
rc=0; (rollback "graft-server did not become healthy" 2>"$tmp/err") || rc=$?
[ "$rc" = 99 ] || fail "upgrade: rollback must fail the deploy (rc $rc)"
grep -q 'rolled back to old, which is up' "$tmp/err" || fail "upgrade: $(cat "$tmp/err")"
[ "$(cat "$meta/meta.db")" = v8-schema ] || fail "upgrade: the old image would open the migrated database"
[ "$(cat "$meta/meta.db-wal")" = v8-wal ] || fail "upgrade: WAL not restored"
[ ! -e "$meta/meta.db-shm" ] || fail "upgrade: the new image's shm was left behind"
[ "$(cat "$here/server.toml")" = old-config ] || fail "upgrade: config not rolled back"
[ "$(grep -n 'stop graft-server' "$tmp/compose" | tail -1 | cut -d: -f1)" -lt \
  "$(grep -n '^up -d' "$tmp/compose" | cut -d: -f1)" ] || fail "upgrade: restored after the old image started"
echo "ok: a new image: the database is backed up first and restored before the old image starts"

# --- the same image again: no backup, a rollback restores nothing -----------
new_host same
printf 'current\n' >"$meta/meta.db"
RUNNING=new GRAFT_IMAGE=new
backup_meta
[ "$meta_backed_up" = 0 ] || fail "same: backed up a redeploy of the running image"
[ ! -e "$meta/pre-deploy" ] || fail "same: wrote a backup"
(rollback "x" 2>/dev/null) || true
[ "$(cat "$meta/meta.db")" = current ] || fail "same: rollback touched the database"
echo "ok: the running image again: no backup, nothing restored"

# --- a fresh host: nothing running, nothing to back up ----------------------
new_host fresh
RUNNING='' GRAFT_IMAGE=new
backup_meta
[ "$meta_backed_up" = 0 ] || fail "fresh: backed up with no server"
[ ! -s "$tmp/compose" ] || fail "fresh: compose was called"
echo "ok: a fresh host: no backup"
