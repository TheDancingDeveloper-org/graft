#!/usr/bin/env bash
# wait_for_refs is eval'd from up.sh, so its uses of these are invisible here.
# shellcheck disable=SC2034,SC2329
# Regression for WI-2485: wait_for_refs under set -e -o pipefail must survive
# an empty refs list (grep finding no "name" exits 1).
#   empty-then-name: [] once, then a ref -> logs importing, returns 0
#   stays-empty:     [] past the deadline -> dies "no ref after import"
#   refs-failure:    refs fails -> dies "endpoint failed"
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

export GRAFT_REFS_POLL=0
base="http://stub"
log() { printf 'up.sh: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }
eval "$(sed -n '/^wait_for_refs()/,/^}/p' "$root/deploy/up.sh")"

calls="$tmp/calls"
mode=
refs_wait=0
rc=0
refs() {
  local n
  n=$(($(cat "$calls" 2>/dev/null || echo 0) + 1))
  echo "$n" >"$calls"
  case $mode in
    empty-then-name) if [ "$n" -lt 2 ]; then printf '[]'; else printf '[{"name":"main"}]'; fi ;;
    stays-empty) printf '[]' ;;
    refs-failure) echo "curl: (22) 500" >&2; return 22 ;;
  esac
}

# Run under up.sh's shell options, outside any if/&&, so set -e is live.
run() {
  mode=$1 refs_wait=$2
  rm -f "$calls"
  set +e
  (set -e -o pipefail; wait_for_refs graft-mirror-x x token) >"$tmp/out" 2>&1
  rc=$?
  set -e
}

fail=0
check() {
  if ! grep -q "$2" "$tmp/out"; then
    echo "FAIL $1: missing '$2'"
    cat "$tmp/out"
    fail=1
  fi
}

run empty-then-name 30
[ "$rc" -eq 0 ] || { echo "FAIL empty-then-name: rc=$rc"; fail=1; }
check empty-then-name 'is importing (no refs yet)'
check empty-then-name 'has 1 ref(s)'

run stays-empty 0
[ "$rc" -ne 0 ] || { echo "FAIL stays-empty: rc=0"; fail=1; }
check stays-empty 'is importing (no refs yet)'
check stays-empty 'no ref after import'

run refs-failure 30
[ "$rc" -ne 0 ] || { echo "FAIL refs-failure: rc=0"; fail=1; }
check refs-failure 'endpoint failed'

if [ "$fail" -eq 0 ]; then echo "up-refs-wait: all cases passed"; fi
exit "$fail"
