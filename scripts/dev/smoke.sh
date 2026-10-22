#!/usr/bin/env bash
# Clone, log, diff and blame the dev repository through the client toolbox.
# Exits 0 when all four pass, 2 while the binaries are stubs, 1 on any other failure.
#
# usage: scripts/dev/smoke.sh [repo-name]   (default: graft-dev)
set -uo pipefail

# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

repo="${1:-graft-dev}"
status=0
stub=0

# Run one graft command in the client toolbox and report its outcome.
# usage: run <label> <graft args...>
run() {
  local label="$1"; shift
  local out rc
  out="$(docker compose --profile tools run --rm client "$@" 2>&1)"
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

run clone graft clone "$repo" "/work/$repo"
run log graft log --json
run diff graft diff
run blame graft blame README.md

if [ "$stub" -eq 1 ]; then
  echo "smoke: not implemented yet: the graft CLI and graft-server are stubs (docs/plan.md)" >&2
  exit 2
fi
exit "$status"
