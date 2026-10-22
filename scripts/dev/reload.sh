#!/usr/bin/env bash
# Build static musl binaries on the host, copy them into running dev containers
# and restart them. The fast loop that works with a remote Docker daemon:
# docker cp goes through the API, so the daemon never needs to see the checkout
# (docs/dev-stack.md).
#
# Binaries are static (x86_64-unknown-linux-musl), so they run in the trixie
# runtime whatever glibc the host has. Needs the musl target installed.
#
# usage: scripts/dev/reload.sh <service>...
#   e.g. scripts/dev/reload.sh graft-server graft-node
#   A `-dev` suffix is accepted and maps to the same binary.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)" || exit 1
cd "$root" || exit 1

target="x86_64-unknown-linux-musl"
out_dir="target/$target/debug"

if [ "$#" -eq 0 ]; then
  sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
  exit 64
fi

# Validate the service names before building anything.
bins=()
for svc in "$@"; do
  bin="${svc%-dev}"
  case "$bin" in
    graft-server|graft-node|graft-mirror) bins+=("$bin") ;;
    *) echo "reload.sh: no graft binary for service '$svc'" >&2; exit 64 ;;
  esac
done

if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
  echo "reload.sh: the $target target is not installed. Run:" >&2
  echo "  rustup target add $target" >&2
  exit 1
fi

cargo build --target "$target" \
  -p graft-server -p graft-node -p graft-mirror -p graft-cli

for bin in "${bins[@]}"; do
  src="$out_dir/$bin"
  if [ ! -f "$src" ]; then
    echo "reload.sh: $src was not built" >&2
    exit 1
  fi
done

# A mirror that refuses its config exits, and on-failure restarts it a few
# times before it gives up. Wait briefly, then fail loudly if the container is
# not running, and show its last log lines (the refusal is printed there).
# usage: check_mirror <service>
check_mirror() {
  local svc="$1" id
  sleep "${RELOAD_SETTLE_SECS:-3}"
  id="$(docker compose --profile '*' ps --status running -q "$svc" 2>/dev/null)"
  if [ -z "$id" ]; then
    echo "reload.sh: $svc is not running: it exited or refused its config." >&2
    echo "reload.sh: last log lines from $svc:" >&2
    docker compose --profile '*' logs --no-color --tail 20 "$svc" 2>&1 | sed 's/^/  /' >&2
    return 1
  fi
}

status=0
for svc in "$@"; do
  bin="${svc%-dev}"
  src="$out_dir/$bin"
  # The container user (uid 10001) must be able to execute the copy.
  chmod a+rx "$src"
  # Copy into the graft-owned dev dir, then restart so the new binary runs.
  docker compose --profile '*' cp "$src" "$svc:/opt/graft-dev/bin/$bin"
  docker compose --profile '*' restart "$svc"
  echo "reload.sh: $svc now runs $src"
  if [ "$bin" = graft-mirror ]; then
    if check_mirror "$svc"; then
      echo "reload.sh: $svc is running"
    else
      status=1
    fi
  fi
done
exit "$status"
