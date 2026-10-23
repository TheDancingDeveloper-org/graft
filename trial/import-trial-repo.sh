#!/usr/bin/env bash
# Create the repository rusttorrent-trial on graft-dev and import rustTorrent
# main into it. The line stays graft-primary and nothing is exported: only
# `graft-mirror import` runs, never `follow` or `export`.
#
#   trial/import-trial-repo.sh
#
# Runs on the VM through `tailscale ssh sprooty@graft-dev`. A short-lived
# admin token is minted into a root-owned temp dir on the VM, used by one
# throwaway container from the stack's own image, and deleted. Nothing secret
# is printed or put in argv.
#
# Land policy: the server gives `main` of every repository the decision-D6
# policy (`LineConfig::d6`, graft-engine): attested on ci/build and ci/test,
# graft-primary. There is no per-repo setting to change, so this script
# checks nothing about it; see the note at the end.
#
# Idempotent: repo create is a no-op for an existing repo, and re-importing
# an unchanged history sets nothing.
#
#   GRAFT_TRIAL_HOST    ssh target (default sprooty@graft-dev)
#   GRAFT_TRIAL_UPSTREAM  git URL to import (default rustTorrent on GitHub)
set -euo pipefail

host=${GRAFT_TRIAL_HOST:-sprooty@graft-dev}
upstream=${GRAFT_TRIAL_UPSTREAM:-https://github.com/TheDancingDeveloper-org/rustTorrent.git}
repo=rusttorrent-trial

[[ $upstream =~ ^https://[A-Za-z0-9./_-]+$ ]] || { echo "import-trial-repo: bad upstream URL" >&2; exit 2; }
command -v tailscale >/dev/null || { echo "import-trial-repo: tailscale is required" >&2; exit 1; }

# The remote script reads its two settings from the positional parameters.
tailscale ssh "$host" bash -s -- "$repo" "$upstream" <<'REMOTE'
set -euo pipefail
repo=$1
upstream=$2
log() { printf 'import-trial-repo: %s\n' "$*" >&2; }

docker exec graft-dev-server graft-server admin repo create "$repo"

image=$(docker inspect -f '{{.Config.Image}}' graft-dev-server)
work=$(sudo mktemp -d /etc/graft/import.XXXXXX)
trap 'sudo rm -rf "$work"' EXIT
sudo chown 10001:10001 "$work"
sudo chmod 0700 "$work"

log "minting a one-hour import token"
docker exec graft-dev-server graft-server admin token mint \
  --subject trial-import --kind service --scope "repo:$repo:admin" --ttl 3600 \
  | sudo tee "$work/token" >/dev/null
sudo chown 10001:10001 "$work/token"
sudo chmod 0600 "$work/token"

log "cloning $upstream and importing main"
sudo docker run --rm --user 10001:10001 --network graft-dev_graft-dev \
  --entrypoint sh -v "$work:/w" "$image" -c '
    set -e
    git clone --quiet --bare --single-branch --branch main "$1" /w/src.git
    graft-mirror import /w/src.git --repo "$2" --no-tags \
      --db /w/map.sqlite --server http://graft-server:7878 --token-file /w/token
  ' sh "$upstream" "$repo"
log "imported into $repo (graft-primary; no export configured)"
REMOTE

cat >&2 <<NOTE
import-trial-repo: policy note. main on $repo is attested (ci/build, ci/test)
by the server default; a land is refused (exit 5) until both checks carry a
passing attestation on the stack head. CI posts them with \`graft attest\`.
NOTE
