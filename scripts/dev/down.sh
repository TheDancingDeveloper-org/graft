#!/usr/bin/env bash
# Stop and remove the graft dev stack (docs/dev-stack.md).
#
# usage: scripts/dev/down.sh [--wipe]
#   default  remove containers and networks; keep the named volumes
#   --wipe   also remove the graft-dev named volumes (objects, metadata, cache)
set -euo pipefail

# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

volumes=()
for arg in "$@"; do
  case "$arg" in
    --wipe) volumes=(--volumes) ;;
    -h|--help)
      sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "down.sh: unknown option: $arg" >&2
      exit 64
      ;;
  esac
done

docker compose --profile '*' down --remove-orphans "${volumes[@]}"
