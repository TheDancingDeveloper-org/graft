#!/usr/bin/env bash
# Deploy the graft-dev stack (deploy/compose.yml) on this host. Idempotent: run
# it by hand or from .github/workflows/deploy-dev.yml; a second run with
# nothing changed leaves the containers alone.
#
#   1. Bootstrap secrets under /etc/graft, if missing: the server signing key,
#      and (after the server is up) the mirror's service token. Both are
#      written by a throwaway root container from the pinned image, mode 0600,
#      owned by uid 10001. Neither is ever printed.
#   2. Pull the pinned image and `compose up -d --wait`.
#   3. Create the repositories `rusttorrent` and `graft-dev` (a retry of an
#      existing repo is a success), mint the mirror token if absent or due for renewal.
#   4. Health-check /healthz, then the /v1 facade: the refs of `rusttorrent`.
#
# With GRAFT_ATTESTER=1 (WI-2276; off by default) it also turns on the compose
# profile `attester`: it creates the repository `rusttorrent-trial`, mints the
# attester's token (`read` and `attest` on that repository, 0600, owned by
# root so the checks the attester runs as uid 10001 can't read it), builds
# the attester image from this checkout and starts it.
#
# Needs: docker with compose v2, curl, and membership of the docker group.
# Non-secret host settings (GRAFT_BIND_IP, ports) may go in
# /etc/graft/deploy.env. Environment overrides:
#   GRAFT_SECRETS_DIR       secrets directory (default /etc/graft)
#   GRAFT_REMINT=1          mint a new mirror token even if the current one is fresh
#   GRAFT_TOKEN_RENEW_DAYS  re-mint a token older than this (default 20; TTL is 30)
#   GRAFT_REFS_WAIT         seconds to wait for rusttorrent's first ref (default 600)
#   GRAFT_ATTESTER=1        run the trial attester (profile `attester`)
#   GRAFT_ATTESTER_REPO     the repository it attests (default rusttorrent-trial)
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
secrets=${GRAFT_SECRETS_DIR:-/etc/graft}
export GRAFT_SECRETS_DIR=$secrets

# An unreadable deploy.env is an error, not a skip: skipping it would move the
# ports between the tailnet address and loopback depending on who runs this.
if [ -e "$secrets/deploy.env" ] && [ ! -r "$secrets/deploy.env" ]; then
  printf 'up.sh: error: %s exists but is not readable by %s\n' "$secrets/deploy.env" "$(id -un)" >&2
  exit 1
fi
if [ -r "$secrets/deploy.env" ]; then
  set -a
  # shellcheck source=/dev/null
  . "$secrets/deploy.env"
  set +a
fi
set -a
# shellcheck source=/dev/null
. "$here/image.env"
set +a

repo=rusttorrent
# Every repository the stack serves; creating one that exists is a no-op.
repos=(rusttorrent graft-dev)
ttl=2592000
attester=${GRAFT_ATTESTER:-0}
attester_repo=${GRAFT_ATTESTER_REPO:-rusttorrent-trial}
export GRAFT_ATTESTER_REPO=$attester_repo
if [ "$attester" = 1 ]; then
  export COMPOSE_PROFILES=attester
  repos+=("$attester_repo")
fi
renew_days=${GRAFT_TOKEN_RENEW_DAYS:-20}
refs_wait=${GRAFT_REFS_WAIT:-600}
host=${GRAFT_BIND_IP:-127.0.0.1}
[ "$host" = 0.0.0.0 ] && host=127.0.0.1
base="http://$host:${GRAFT_SERVER_PORT:-17888}"

log() { printf 'up.sh: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

compose() {
  docker compose --env-file "$here/image.env" -f "$here/compose.yml" "$@"
}

# Run a shell snippet as root in a throwaway container from the pinned image,
# with the secrets directory at /s. stdin is passed through. No network.
as_root() {
  docker run --rm -i --user 0:0 --network none --entrypoint sh \
    -v "$secrets:/s" "$GRAFT_IMAGE" -c "$1"
}

# Write stdin to /s/$1 atomically: 0600, owned by $2 (default 10001:10001),
# refusing an empty value.
store_secret() {
  as_root "set -e
    umask 077
    cat > /s/.$1.tmp
    if [ ! -s /s/.$1.tmp ]; then rm -f /s/.$1.tmp; echo 'empty value refused' >&2; exit 1; fi
    chown ${2:-10001:10001} /s/.$1.tmp
    chmod 0600 /s/.$1.tmp
    mv /s/.$1.tmp /s/$1"
}

command -v curl >/dev/null || die "curl is required"

log "pulling $GRAFT_IMAGE"
# The attester (profile `attester`) is built here, not pulled.
compose pull --quiet graft-server graft-mirror

if [ ! -e "$secrets/signing.key" ]; then
  log "bootstrap: creating $secrets/signing.key"
  as_root "head -c 32 /dev/urandom" | store_secret signing.key
fi

GRAFT_CONFIG_HASH=$(cat "$here/server.toml" "$here/mirror.toml" | sha256sum | cut -c1-16)
export GRAFT_CONFIG_HASH

log "starting graft-server"
compose up -d --wait graft-server

for name in "${repos[@]}"; do
  compose exec -T graft-server graft-server admin repo create "$name" >&2
done

# Whether the token file $1 is missing or due for renewal ("what" names it
# in the log).
due() {
  local file="$secrets/$1" what=$2
  if [ ! -e "$file" ]; then
    log "bootstrap: minting the $what into $file"
  elif [ "${GRAFT_REMINT:-0}" = 1 ]; then
    log "GRAFT_REMINT=1: re-minting the $what"
  elif [ -n "$(find "$file" -mtime "+$renew_days" 2>/dev/null)" ]; then
    log "$what is older than $renew_days days: re-minting"
  else
    return 1
  fi
}

mint=0
due mirror-token "mirror token" && mint=1
# Whether the mirror already runs: only then does a new token need a restart.
mirror_was_up=$(compose ps -q graft-mirror)
if [ "$mint" = 1 ]; then
  compose exec -T graft-server graft-server admin token mint \
    --subject graft-mirror-rusttorrent --kind service \
    --scope "repo:$repo:admin" --ttl "$ttl" \
    | store_secret mirror-token
fi

attester_mint=0
attester_was_up=
if [ "$attester" = 1 ]; then
  due attester-token "attester token" && attester_mint=1
  attester_was_up=$(compose ps -q graft-attester)
  if [ "$attester_mint" = 1 ]; then
    # `attest` grants Attest only, so the attester also needs `read` to
    # watch the journal and check drafts out. Owned by root: see compose.yml.
    compose exec -T graft-server graft-server admin token mint \
      --subject "graft-attester-$attester_repo" --kind service \
      --scope "repo:$attester_repo:read" --scope "repo:$attester_repo:attest" \
      --ttl "$ttl" \
      | store_secret attester-token 0:0
  fi
  log "building the attester image from $here/.."
  compose build graft-attester
fi

log "starting the stack"
compose up -d --wait --remove-orphans
if [ "$mint" = 1 ] && [ -n "$mirror_was_up" ]; then
  # A new token for a mirror that was running: restart it so it reads it.
  compose up -d --wait --no-deps --force-recreate graft-mirror
fi
if [ "$attester_mint" = 1 ] && [ -n "$attester_was_up" ]; then
  compose up -d --wait --no-deps --force-recreate graft-attester
fi

log "health: $base/healthz"
curl -fsS -o /dev/null "$base/healthz" || die "$base/healthz failed"

# The /v1 facade, authenticated with the mirror token. The token goes to curl
# on stdin (-H @-), never in arguments, and is never printed.
refs() {
  as_root "cat /s/mirror-token" \
    | sed 's/^/Authorization: Bearer /' \
    | curl -fsS -H @- "$base/v1/repos/$repo/refs?limit=100"
}

log "health: $base/v1/repos/$repo/refs (waiting up to ${refs_wait}s for a ref)"
deadline=$((SECONDS + refs_wait))
while :; do
  if body=$(refs); then
    count=$(printf '%s' "$body" | grep -o '"name"' | wc -l)
    if [ "$count" -gt 0 ]; then
      log "ok: $repo has $count ref(s); stack is up"
      break
    fi
  else
    log "/v1 refs request failed; retrying"
  fi
  [ "$SECONDS" -lt "$deadline" ] || die "$repo has no refs after ${refs_wait}s (see: docker logs graft-dev-mirror)"
  sleep 10
done
