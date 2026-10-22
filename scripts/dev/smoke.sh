#!/usr/bin/env bash
# Check every started mirror is running, then clone, log, diff and blame the
# dev repository through the client toolbox.
# Exits 0 when all four pass, 2 while the binaries are stubs, 1 on any other failure.
#
# usage: scripts/dev/smoke.sh [repo-name]   (default: graft-dev)
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)" || exit 1
cd "$root" || exit 1

repo="${1:-graft-dev}"
status=0
stub=0

# Run one graft command in the client toolbox and report its outcome. Each run
# is a fresh container, so the working directory is set per call.
# usage: run <label> <workdir> <graft args...>
run() {
  local label="$1" dir="$2"; shift 2
  local out rc
  out="$(docker compose --profile tools run --rm -w "$dir" client "$@" 2>&1)"
  rc=$?
  if grep -q "not implemented yet" <<<"$out"; then
    stub=1
    echo "smoke: $label: not implemented yet"
  elif [ "$rc" -ne 0 ]; then
    status=1
    echo "smoke: $label exited $rc:" >&2
    echo "$out" >&2
  else
    echo "smoke: $label ok"
  fi
}

# A mirror that refused its config exits and on-failure gives up, so a mirror
# service that was started but is no longer running is a failure. Its last log
# lines carry the refusal.
for svc in $(docker compose --profile '*' ps -a --format '{{.Service}}' 2>/dev/null | grep '^graft-mirror'); do
  if [ -z "$(docker compose --profile '*' ps --status running -q "$svc" 2>/dev/null)" ]; then
    status=1
    echo "smoke: $svc is not running: it exited or refused its config. Last log lines:" >&2
    docker compose --profile '*' logs --no-color --tail 20 "$svc" 2>&1 | sed 's/^/  /' >&2
  else
    echo "smoke: $svc running"
  fi
done

run clone /work graft clone "$repo" "/work/$repo"
run log "/work/$repo" graft log --json
run diff "/work/$repo" graft diff
run blame "/work/$repo" graft blame README.md

if [ "$stub" -eq 1 ]; then
  echo "smoke: not implemented yet: the graft CLI and graft-server are stubs (docs/plan.md)" >&2
  exit 2
fi
exit "$status"
