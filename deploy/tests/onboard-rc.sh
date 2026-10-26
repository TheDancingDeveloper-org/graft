#!/usr/bin/env bash
# Exercise the line-create retry loop in onboard-repo.sh with a stubbed
# docker (WI-2483): exit 0 -> success, 1 -> die, 75 -> retry until attempts run out.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script="$here/../onboard-repo.sh"
loop=$(sed -n '/^attempts=/,/^done$/p' "$script")
[[ -n $loop ]] || { echo "could not find the line-create loop" >&2; exit 1; }

# shellcheck disable=SC2329,SC2154  # stubs and rc are used by the eval'd loop
run() { # $1 = docker exit status; prints the outcome and docker call count
  (
    set +e
    calls=0
    docker() { calls=$((calls + 1)); return "$STUB_RC"; }
    log() { :; }
    die() { echo "died after $calls"; exit 1; }
    export STUB_RC=$1 GRAFT_IMAGE=stub GRAFT_LINE_ATTEMPTS=3 GRAFT_LINE_POLL=0
    # shellcheck disable=SC2034  # used by the eval'd loop
    work=/nonexistent repo=r base=b line=l
    eval "$loop"
    echo "ok after $calls rc=$rc"
  )
}

fail=0
check() {
  local got
  got=$(run "$1") || true
  if [[ $got == "$2" ]]; then
    echo "pass: docker exit $1 -> $got"
  else
    echo "FAIL: docker exit $1 -> '$got', want '$2'"
    fail=1
  fi
}
check 0 'ok after 1 rc=0'
check 1 'died after 1'
check 75 'died after 3'
exit "$fail"
