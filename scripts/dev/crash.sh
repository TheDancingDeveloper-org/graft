#!/usr/bin/env bash
# Crash test: kill -9 graft-server while smoke runs, let the restart policy bring
# it back, then verify with smoke again. Needs the real server (exits 2 while stubbed).
#
# usage: scripts/dev/crash.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

# Find the running server container: dev variant when up, else the image one.
cid="$(docker compose --profile '*' ps -q --status running graft-server-dev graft-server 2>/dev/null | head -n 1)"
if [ -z "$cid" ]; then
  echo "crash: graft-server is not running (stubs exit at start; see docs/dev-stack.md)" >&2
  exit 2
fi

scripts/dev/smoke.sh > /dev/null 2>&1 &
load=$!

docker kill --signal KILL "$cid" > /dev/null
echo "crash: killed $cid with SIGKILL mid-load"

# The restart policy (on-failure:3) brings the server back. Poll for up to 60s.
for _ in $(seq 60); do
  now="$(docker compose --profile '*' ps -q --status running graft-server-dev graft-server 2>/dev/null | head -n 1)"
  [ -n "$now" ] && break
  sleep 1
done
if [ -z "${now:-}" ]; then
  echo "crash: graft-server did not come back within 60s" >&2
  exit 1
fi
echo "crash: graft-server is back"

wait "$load" || true
scripts/dev/smoke.sh
