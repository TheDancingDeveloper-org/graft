#!/usr/bin/env bash
# Start the graft dev stack (docs/dev-stack.md).
#
# usage: scripts/dev/up.sh [--dev] [--mirror] [--rusttorrent] [--obs]
#   default      graft-server and graft-node from graft-dev:local
#   --dev        dev profile: host binaries from target/debug (run cargo build first)
#   --mirror     add graft-mirror following the fixture repository
#   --rusttorrent add graft-mirror-rusttorrent (needs GitHub access)
#   --obs        add prometheus and grafana on 127.0.0.1
# A remote daemon (Node B) is supported: lib.sh maps the checkout path (GRAFT_HOST_ROOT).
set -euo pipefail

# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

mode=image
extra=()
for arg in "$@"; do
  case "$arg" in
    --dev) mode=dev ;;
    --mirror) extra+=(--profile mirror) ;;
    --rusttorrent) extra+=(--profile rusttorrent) ;;
    --obs) extra+=(--profile obs) ;;
    -h|--help)
      sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "up.sh: unknown option: $arg" >&2
      exit 64
      ;;
  esac
done

if [ "$mode" = dev ]; then
  for bin in graft-server graft-node graft-mirror; do
    if [ ! -f "target/debug/$bin" ]; then
      echo "up.sh: target/debug/$bin is missing; run 'cargo build -p graft-server -p graft-node -p graft-mirror' first" >&2
      exit 1
    fi
    # The container runs as uid 10001; a 0700 build output would not start.
    chmod a+rx "target/debug/$bin"
  done
fi

# --remove-orphans clears containers from the other mode. In dev mode the
# default server and node are scaled to 0, so they never hold port 17878 or the
# node socket while the host-binary variants run.
if [ "$mode" = dev ]; then
  docker compose --profile dev "${extra[@]}" up -d --remove-orphans \
    --scale graft-server=0 --scale graft-node=0
else
  docker compose "${extra[@]}" up -d --remove-orphans
fi
docker compose --profile '*' ps
