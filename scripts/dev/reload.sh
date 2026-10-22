#!/usr/bin/env bash
# Copy host debug binaries into running dev containers and restart them.
# The fast loop that works with a remote Docker daemon: docker cp goes through
# the API, so the daemon never needs to see the checkout (docs/dev-stack.md).
#
# usage: scripts/dev/reload.sh <service>...
#   e.g. scripts/dev/reload.sh graft-server graft-node
#   A `-dev` suffix is accepted and maps to the same binary.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

if [ "$#" -eq 0 ]; then
  sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
  exit 64
fi

for svc in "$@"; do
  bin="${svc%-dev}"
  case "$bin" in
    graft-server|graft-node|graft-mirror) ;;
    *) echo "reload.sh: no graft binary for service '$svc'" >&2; exit 64 ;;
  esac
  src="target/debug/$bin"
  if [ ! -f "$src" ]; then
    echo "reload.sh: $src is missing; run 'cargo build -p $bin' first" >&2
    exit 1
  fi
  # The container user (uid 10001) must be able to execute the copy.
  chmod a+rx "$src"
  # Copy into the graft-owned dev dir, then restart so the new binary runs.
  docker compose --profile '*' cp "$src" "$svc:/opt/graft-dev/bin/$bin"
  docker compose --profile '*' restart "$svc"
  echo "reload.sh: $svc now runs $src"
done
