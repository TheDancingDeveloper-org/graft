#!/usr/bin/env bash
# Create the integration line `trial-1` of the followed repository
# `rusttorrent` on graft-dev, forked at the current tip of rustTorrent main
# (decision D21). The mirror keeps following main; nothing is exported.
#
#   trial/create-trial-line.sh
#
# graft has no "create line" command, so this uses the mirror's own path:
# `graft-mirror import … --line refs/heads/main=trial-1` of a fresh clone of
# upstream main. Ids are derived from content, so the import reproduces the
# followed history exactly and only the new ref is set. Runs on the VM through
# `tailscale ssh sprooty@graft-dev`. A one-hour admin token lives in a
# root-owned temp dir used by one throwaway container of the stack's image,
# then deleted. Nothing secret is printed or put in argv.
#
# Idempotent: if `trial-1` already exists it is left alone, never moved, so a
# re-run cannot rewind the line. The line's land policy (`direct`, decision
# D22) comes from deploy/server.toml, not from this script.
#
#   GRAFT_TRIAL_HOST      ssh target (default sprooty@graft-dev)
#   GRAFT_TRIAL_UPSTREAM  git URL to fork from (default rustTorrent on GitHub)
set -euo pipefail

host=${GRAFT_TRIAL_HOST:-sprooty@graft-dev}
upstream=${GRAFT_TRIAL_UPSTREAM:-https://github.com/TheDancingDeveloper-org/rustTorrent.git}
repo=rusttorrent
line=trial-1

[[ $upstream =~ ^https://[A-Za-z0-9./_-]+$ ]] || { echo "create-trial-line: bad upstream URL" >&2; exit 2; }
command -v tailscale >/dev/null || { echo "create-trial-line: tailscale is required" >&2; exit 1; }

# The remote script reads its settings from the positional parameters.
tailscale ssh "$host" bash -s -- "$repo" "$line" "$upstream" <<'REMOTE'
set -euo pipefail
repo=$1
line=$2
upstream=$3
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
    repo=$1; line=$2; upstream=$3
    g() { graft --endpoint http://graft-server:7878 --token-file /w/token --repo "$repo" "$@"; }
    if g log --at "$line" --limit 1 >/w/tip 2>/w/err; then
      echo "create-trial-line: $line already exists; left alone" >&2
      cat /w/tip
      exit 0
    fi
    # Only "no such line" means create it; anything else (server down,
    # token refused) is a failure, not a reason to import.
    grep -qi "not.found" /w/err || { cat /w/err >&2; exit 1; }
    echo "create-trial-line: cloning $upstream" >&2
    git clone --quiet --bare --single-branch --branch main "$upstream" /w/src.git
    graft-mirror import /w/src.git --repo "$repo" --no-tags --line "refs/heads/main=$line" \
      --db /w/map.sqlite --server http://graft-server:7878 --token-file /w/token
    echo "create-trial-line: $line (tip first) and main (should match):" >&2
    g log --at "$line" --limit 1
    g log --at main --limit 1
  ' sh "$repo" "$line" "$upstream"
REMOTE
