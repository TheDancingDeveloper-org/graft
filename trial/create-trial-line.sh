#!/usr/bin/env bash
# Create the integration line `$GRAFT_TRIAL_LINE` (default `trial-1`) of the
# followed repository `$GRAFT_TRIAL_REPO` (default `rusttorrent`) on
# graft-dev, forked at the current tip of its followed base line
# `$GRAFT_TRIAL_BASE` (default `main`; vogt's is `rust-core`), decision D21.
# The mirror keeps following the base line; nothing is exported.
#
#   trial/create-trial-line.sh
#
# It runs `graft line create <line> --from <base>` on the VM through
# `tailscale ssh sprooty@graft-dev`, in one throwaway container of the
# stack's image. A one-hour admin token lives in a root-owned temp dir, then
# is deleted. Nothing secret is printed or put in argv.
#
# Idempotent: if the line already exists it is left alone, never moved, so a
# re-run cannot rewind the line. The line's land policy (`direct`, decision
# D22) comes from deploy/server.toml, not from this script.
#
#   GRAFT_TRIAL_HOST   ssh target (default sprooty@graft-dev)
#   GRAFT_TRIAL_REPO   graft repository (default rusttorrent)
#   GRAFT_TRIAL_BASE   followed line to fork from (default main)
#   GRAFT_TRIAL_LINE   line to create (default trial-1)
set -euo pipefail

host=${GRAFT_TRIAL_HOST:-sprooty@graft-dev}
repo=${GRAFT_TRIAL_REPO:-rusttorrent}
base=${GRAFT_TRIAL_BASE:-main}
line=${GRAFT_TRIAL_LINE:-trial-1}

die() { echo "create-trial-line: $*" >&2; exit 2; }
# These go into remote commands: accept only safe shapes.
[[ $host =~ ^[A-Za-z0-9._-]+@[A-Za-z0-9._-]+$ ]] || die "bad GRAFT_TRIAL_HOST"
[[ $repo =~ ^[a-z0-9][a-z0-9_-]{0,63}$ ]] || die "bad GRAFT_TRIAL_REPO"
[[ $base =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || die "bad GRAFT_TRIAL_BASE"
[[ $line =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || die "bad GRAFT_TRIAL_LINE"
[ "$line" != "$base" ] || die "GRAFT_TRIAL_LINE must differ from GRAFT_TRIAL_BASE"
command -v tailscale >/dev/null || { echo "create-trial-line: tailscale is required" >&2; exit 1; }

# The remote script reads its settings from the positional parameters.
tailscale ssh "$host" bash -s -- "$repo" "$base" "$line" <<'REMOTE'
set -euo pipefail
repo=$1
base=$2
line=$3
log() { printf 'create-trial-line: %s\n' "$*" >&2; }

image=$(docker inspect -f '{{.Config.Image}}' graft-dev-server)
work=$(sudo mktemp -d /etc/graft/line.XXXXXX)
trap 'sudo rm -rf "$work"' EXIT
sudo chown 10001:10001 "$work"
sudo chmod 0700 "$work"

log "minting a one-hour admin token"
docker exec graft-dev-server graft-server admin token mint \
  --subject trial-line --kind service --scope "repo:$repo:admin" --ttl 3600 \
  | sudo tee "$work/token" >/dev/null
sudo chown 10001:10001 "$work/token"
sudo chmod 0600 "$work/token"

sudo docker run --rm --user 10001:10001 --network graft-dev \
  --entrypoint sh -v "$work:/w" "$image" -c '
    set -eu
    repo=$1; base=$2; line=$3
    g() { graft --endpoint http://graft-server:7878 --token-file /w/token --repo "$repo" "$@"; }
    if g log --at "$line" --limit 1 >/w/tip 2>/w/err; then
      echo "create-trial-line: $line already exists; left alone" >&2
      cat /w/tip
      exit 0
    fi
    # Only "no such line" means create it; anything else (server down,
    # token refused) is a failure, not a reason to create.
    grep -qi "not.found" /w/err || { cat /w/err >&2; exit 1; }
    echo "create-trial-line: forking $line from $base" >&2
    g line create "$line" --from "$base"
    echo "create-trial-line: $line (tip first) and $base (should match):" >&2
    g log --at "$line" --limit 1
    g log --at "$base" --limit 1
  ' sh "$repo" "$base" "$line"
REMOTE
