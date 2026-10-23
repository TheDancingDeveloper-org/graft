#!/usr/bin/env bash
# Restore rehearsal for the graft-dev backup (WI-2307). Copies a backup into a
# scratch directory and runs `graft-server admin fsck` on each repository
# against the copy, from a throwaway container with no network. It never
# mounts the live stack's volumes.
#
#   sudo deploy/backup/restore-rehearsal.sh [backup-dir] [meta-snapshot]
#
# backup-dir defaults to /var/backups/graft-dev; meta-snapshot to the newest
# meta/meta-*.db in it. Repositories: $GRAFT_REPOS (default
# "rusttorrent graft-dev"). The scratch copy is removed afterwards unless
# GRAFT_KEEP_SCRATCH=1.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
deploy=$(cd "$here/.." && pwd)
src=${1:-/var/backups/graft-dev}
meta=${2:-$(find "$src/meta" -maxdepth 1 -name 'meta-*.db' | sort | tail -n1)}
repos=${GRAFT_REPOS:-rusttorrent graft-dev}
[ -n "$meta" ] && [ -f "$meta" ] || { echo "restore-rehearsal: no snapshot in $src/meta" >&2; exit 1; }

set -a
# shellcheck source=/dev/null
. "$deploy/image.env"
set +a

scratch=$(mktemp -d /var/tmp/graft-restore.XXXXXX)
[ "${GRAFT_KEEP_SCRATCH:-0}" = 1 ] || trap 'rm -rf "$scratch"' EXIT
echo "restore-rehearsal: $meta + $src/current/objects -> $scratch" >&2

mkdir -p "$scratch/meta" "$scratch/key"
cp "$meta" "$scratch/meta/meta.db"
cp -a "$src/current/objects" "$scratch/objects"
# fsck opens the stores as serve does, which wants a signing key: a throwaway
# one, since the real key is not in the backup.
head -c 32 /dev/urandom > "$scratch/key/signing.key"
cat > "$scratch/server.toml" <<'TOML'
objects_dir = "/restore/objects"
meta_db = "/restore/meta/meta.db"
key_file = "/restore/key/signing.key"
TOML
chown -R 10001:10001 "$scratch"
chmod 0600 "$scratch/key/signing.key"

rc=0
for repo in $repos; do
  docker run --rm --user 10001:10001 --network none --read-only \
    -v "$scratch:/restore" "$GRAFT_IMAGE" \
    graft-server --config /restore/server.toml admin fsck "$repo" || rc=1
done
exit "$rc"
