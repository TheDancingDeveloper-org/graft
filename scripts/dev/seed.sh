#!/usr/bin/env bash
# Create the dev repository on graft-server and import the fixture git repository
# through graft-mirror. Run once, after up.sh --mirror. Needs the real binaries:
# while they are stubs this exits 2 with the stub's message.
#
# usage: scripts/dev/seed.sh [repo-name]   (default: graft-dev)
set -euo pipefail

# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

repo="${1:-graft-dev}"
fixture=/var/lib/graft/mirror/fixtures/graft-fixture.git

# Run one command in a throwaway container, keep its output, and stop on failure.
step() {
  local name="$1"; shift
  local out
  if ! out="$(docker compose --profile '*' run --rm --no-deps "$@" 2>&1)"; then
    echo "seed: $name failed:" >&2
    echo "$out" >&2
    if grep -q "not implemented yet" <<<"$out"; then
      echo "seed: graft binaries are still stubs (docs/plan.md); nothing was created" >&2
    fi
    exit 2
  fi
  echo "seed: $name ok"
}

step "create repo $repo" graft-server graft-server admin repo create "$repo"
step "import fixture into $repo" graft-mirror graft-mirror import "$fixture" --repo "$repo"
