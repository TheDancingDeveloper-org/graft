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
#   A `-dev` suffix is accepted and maps to the same binary, and any
#   `graft-mirror*` service (graft-mirror-rusttorrent, ...) maps to graft-mirror.
#   scripts/dev/reload.sh --bin <service> prints the binary and exits.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)" || exit 1
cd "$root" || exit 1

target="x86_64-unknown-linux-musl"
out_dir="target/$target/debug"

if [ "$#" -eq 0 ]; then
  sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
  exit 64
fi

# Print the graft binary a compose service runs, or fail.
# usage: service_bin <service>
service_bin() {
  local svc="$1"
  case "${svc%-dev}" in
    graft-server|graft-node) echo "${svc%-dev}" ;;
    graft-mirror|graft-mirror-*) echo graft-mirror ;;
    *) echo "reload.sh: no graft binary for service '$svc'" >&2; return 64 ;;
  esac
}

if [ "$1" = --bin ]; then
  [ "$#" -eq 2 ] || { echo "usage: reload.sh --bin <service>" >&2; exit 64; }
  service_bin "$2" || exit 64
  exit 0
fi

# Validate the service names before building anything.
bins=()
for svc in "$@"; do
  bin="$(service_bin "$svc")" || exit 64
  bins+=("$bin")
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
# A config refusal is immediate, so the settle wait covers it; a mirror that
# fails later (after its first poll) can still pass this check.
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
  bin="$(service_bin "$svc")"
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
