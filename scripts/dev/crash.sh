#!/usr/bin/env bash
# Crash test: kill -9 graft-server while smoke runs, let the restart policy bring
# it back, then verify with smoke again. Needs the real server (exits 2 while stubbed).
#
# The signal goes to the server process, not the container. `docker kill` counts
# as a manual stop, and Docker never restarts a manually stopped container. The
# graft-server service runs with `init: true`, so when the server dies, PID 1
# exits too, the container exits non-zero, and on-failure:3 restarts it.
#
# usage: scripts/dev/crash.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)" || exit 1
cd "$root"

running() {
  docker compose --profile '*' ps -q --status running graft-server-dev graft-server 2>/dev/null | head -n 1
}

# Find the running server container: dev variant when up, else the image one.
cid="$(running)"
if [ -z "$cid" ]; then
  echo "crash: graft-server is not running (stubs exit at start; see docs/dev-stack.md)" >&2
  exit 2
fi

scripts/dev/smoke.sh > /dev/null 2>&1 &
load=$!

# kill -9 the process named graft-server. Reads /proc, so it needs no procps in
# the image. Exits non-zero when no such process exists.
if ! docker exec "$cid" sh -c '
  found=0
  for p in /proc/[0-9]*; do
    if [ "$(cat "$p/comm" 2>/dev/null)" = graft-server ]; then
      kill -9 "${p#/proc/}" && found=1
    fi
  done
  [ "$found" = 1 ]'; then
  echo "crash: no graft-server process in $cid" >&2
  wait "$load" || true
  exit 1
fi
echo "crash: SIGKILLed the graft-server process in $cid mid-load"

# Wait for the container to leave the running state (it exits with the server).
for _ in $(seq 10); do
  [ -z "$(running)" ] && break
  sleep 1
done

# The restart policy (on-failure:3) brings the server back. Poll for up to 60s.
for _ in $(seq 60); do
  now="$(running)"
  [ -n "$now" ] && break
  sleep 1
done
if [ -z "${now:-}" ]; then
  echo "crash: graft-server did not come back within 60s" >&2
  wait "$load" || true
  exit 1
fi
echo "crash: graft-server is back"

wait "$load" || true
scripts/dev/smoke.sh
