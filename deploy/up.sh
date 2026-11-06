#!/usr/bin/env bash
# Deploy the graft-dev stack (deploy/compose.yml) on this host. Idempotent: run
# it by hand (the deploy-dev workflow is retired); a second run with
# nothing changed leaves the containers alone.
#
#   1. Bootstrap secrets under /etc/graft, if missing: the server signing key,
#      and (after the server is up) the mirror service's one `mirror`-kind
#      token. Both are written by a throwaway root container from the pinned
#      image, mode 0600, owned by uid 10001. Neither is ever printed. The
#      credential key `kek-k1` is provisioned by hand (deploy/README.md,
#      mirror-service-cutover.md step 1); this script only fixes its owner and
#      mode, and stops if it is missing.
#   2. Pull the pinned image and pre-flight: check deploy/server.toml and
#      deploy/mirror-service.toml with that image's own binaries. A config the
#      image can't parse stops the deploy here, with the running stack
#      untouched (WI-2291).
#   3. `compose up -d --wait` and wait for /readyz. If the new stack does not
#      come up, put back the last good image pin and configs (kept under
#      $GRAFT_SECRETS_DIR/last-good by the last successful deploy), bring that
#      stack up and exit non-zero. A good deploy records itself as last good.
#   4. Create the repositories `graft-dev` (and the attester's), mint the
#      mirror service's token if absent or due for renewal. The followed
#      repositories are mirror rows, not deploy state: `graft repo mirror add`
#      (operator) onboards one; this script creates none.
#   5. Health-check /healthz. The mirror service is part of `--wait` (its
#      /metrics health check); `graft repo mirror status` shows each row.
#
# With GRAFT_ATTESTER=1 (WI-2276; off by default) it also turns on the compose
# profile `attester`: it mints the attester's token (`read` and `attest` on
# `rusttorrent`, whose line `trial-1` it attests (decision D21); 0600, owned
# by root so the checks the attester runs as uid 10001 can't read it), builds
# the attester image from this checkout and starts it.
#
# With GRAFT_POST_LAND=1 (v0.2 exit; off by default) it also turns on the
# compose profile `post-land`: it mints the post-land checker's token into
# post-land-token (`read`, `attest` and `write` on `graft`, and `land` on
# the checked line; 0600, uid 10001), builds the checker image (target
# `checker`) and starts it. The checker has its own GitHub token,
# github-checker-token (a token of its own): placed from
# Infisical when this host has the `secret` tool, else the copy already on
# the host. With neither, the checker stays off and the deploy continues.
# docs/runbooks/v02-exit.md.
#
# Needs: docker with compose v2, curl, and membership of the docker group.
# Non-secret host settings (GRAFT_BIND_IP, ports) may go in
# /etc/graft/deploy.env. Environment overrides:
#   GRAFT_SECRETS_DIR       secrets directory (default /etc/graft)
#   GRAFT_REMINT=1          mint a new mirror service token even if the current one is fresh
#   GRAFT_TOKEN_RENEW_DAYS  re-mint a token older than this (default 20; TTL is 30)
#   GRAFT_ATTESTER=1        run the trial attester (profile `attester`)
#   GRAFT_ATTESTER_REPO     the repository it attests (default rusttorrent)
#   GRAFT_ATTESTER_LINE     the line whose drafts it attests (default trial-1)
#   GRAFT_POST_LAND=1       run the post-land checker (profile `post-land`)
#   GRAFT_POST_LAND_LINE    the line of `graft` it checks (default
#                           exit-rehearsal); its token's `land` scope names
#                           it, so changing it needs a new post-land-token
#   GRAFT_CHECKER_GITHUB_SECRET  Infisical name of the checker's GitHub token
#                           (default GRAFT_DEV_CHECKER_GITHUB_TOKEN)
#   GRAFT_CHECKER_GITHUB_PROJECT its Infisical project (default apps)
#   GRAFT_READY_WAIT        seconds to wait for /readyz after a swap (default 120)
#
# Exit status: 0 deployed; 1 failed. A failed pre-flight or a rollback says so
# on stderr and, under GitHub Actions, as an ::error:: annotation.
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

# Every repository the deploy creates; creating one that exists is a no-op.
# Mirrored repositories are created with their mirror row, not here.
repos=(graft-dev)
ttl=2592000
attester=${GRAFT_ATTESTER:-0}
attester_repo=${GRAFT_ATTESTER_REPO:-rusttorrent}
export GRAFT_ATTESTER_REPO=$attester_repo
profiles=()
if [ "$attester" = 1 ]; then
  profiles+=(attester)
  # Created if it is not one the stack already serves (the default is).
  case " ${repos[*]} " in
    *" $attester_repo "*) ;;
    *) repos+=("$attester_repo") ;;
  esac
fi
post_land=${GRAFT_POST_LAND:-0}
post_land_line=${GRAFT_POST_LAND_LINE:-exit-rehearsal}
export GRAFT_POST_LAND_LINE=$post_land_line
if [ "$post_land" = 1 ]; then
  profiles+=(post-land)
  [[ $post_land_line =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] \
    || { printf 'up.sh: error: bad GRAFT_POST_LAND_LINE\n' >&2; exit 1; }
fi
set_profiles() {
  if [ "${#profiles[@]}" -gt 0 ]; then
    COMPOSE_PROFILES=$(IFS=,; printf '%s' "${profiles[*]}")
    export COMPOSE_PROFILES
  else
    unset COMPOSE_PROFILES
  fi
}
set_profiles
renew_days=${GRAFT_TOKEN_RENEW_DAYS:-20}
ready_wait=${GRAFT_READY_WAIT:-120}
host=${GRAFT_BIND_IP:-127.0.0.1}
[ "$host" = 0.0.0.0 ] && host=127.0.0.1
base="http://$host:${GRAFT_SERVER_PORT:-17888}"

log() { printf 'up.sh: %s\n' "$*" >&2; }
die() {
  log "error: $*"
  if [ "${GITHUB_ACTIONS:-}" = true ]; then
    printf '::error title=deploy-dev::%s\n' "$*"
  fi
  exit 1
}

compose() {
  docker compose --env-file "$here/image.env" -f "$here/compose.yml" "$@"
}

# Run a shell snippet as root in a throwaway container from the pinned image,
# with the secrets directory at /s. stdin is passed through. No network.
# as_root_mounts adds extra docker mounts for one call.
as_root_mounts=()
as_root() {
  docker run --rm -i --user 0:0 --network none --entrypoint sh \
    -v "$secrets:/s" "${as_root_mounts[@]}" "$GRAFT_IMAGE" -c "$1"
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

# The post-land checker's own GitHub token (operator decision: not the
# former shared mirror github-token, which had far broader scope). A fine-grained token,
# graft repository only, Actions read/write and Metadata read, kept in
# Infisical as $GRAFT_CHECKER_GITHUB_SECRET. When this host has the `secret`
# tool, the value is piped from it into $secrets/github-checker-token
# through store_secret (stdin only, 0600, uid 10001); a failed or empty read
# leaves any existing file alone. Otherwise the file placed on the host by
# hand is used, set to 0600 and uid 10001 by the root helper. The runner
# never reads it, and only the checker mounts it.
checker_github_ready=0
install_checker_github_token() {
  local name=${GRAFT_CHECKER_GITHUB_SECRET:-GRAFT_DEV_CHECKER_GITHUB_TOKEN}
  local project=${GRAFT_CHECKER_GITHUB_PROJECT:-apps}
  checker_github_ready=0
  if command -v secret >/dev/null 2>&1; then
    if secret get "$name" --project "$project" 2>/dev/null \
      | store_secret github-checker-token 2>/dev/null; then
      checker_github_ready=1
      log "github-checker-token placed from Infisical ($project/$name; mode 0600, uid 10001, not printed)"
      return 0
    fi
    log "Infisical gave no value for $project/$name"
  fi
  if as_root "set -e
    test -s /s/github-checker-token
    test -n \"\$(tr -d ' \t\r\n' < /s/github-checker-token)\"
    chown 10001:10001 /s/github-checker-token
    chmod 0600 /s/github-checker-token" 2>/dev/null; then
    checker_github_ready=1
    log "github-checker-token: using the copy on this host (mode 0600, uid 10001, not printed)"
  fi
}

# The credential key (mirror-service-cutover.md step 1). Never created here:
# a key minted by a deploy and lost on a rebuild would orphan every sealed
# credential, so the operator provisions it and backs it up first. The root
# helper only sets the owner and mode the server (uid 10001) reads it with.
install_kek() {
  as_root "set -e
    test -s /s/kek-k1
    chown 10001:10001 /s/kek-k1
    chmod 0600 /s/kek-k1" 2>/dev/null \
    || die "$secrets/kek-k1 is missing or empty: provision the credential key first (deploy/mirror-service-cutover.md step 1), and keep a copy in Infisical"
}

install_kek
if [ "$post_land" = 1 ]; then
  install_checker_github_token
  if [ "$checker_github_ready" != 1 ]; then
    log "warning: GRAFT_POST_LAND=1 but the checker's GitHub token is absent (Infisical ${GRAFT_CHECKER_GITHUB_PROJECT:-apps}/${GRAFT_CHECKER_GITHUB_SECRET:-GRAFT_DEV_CHECKER_GITHUB_TOKEN}, or $secrets/github-checker-token): the post-land checker stays disabled; see docs/runbooks/v02-exit.md"
    post_land=0
    keep=()
    for p in "${profiles[@]}"; do [ "$p" = post-land ] || keep+=("$p"); done
    profiles=("${keep[@]}")
    set_profiles
    # A checker left running from an earlier deploy has lost its credential.
    if [ -n "$(docker ps -q --filter name=^graft-dev-post-land\$)" ]; then
      log "stopping graft-dev-post-land"
      docker rm -f graft-dev-post-land >/dev/null
    fi
  fi
fi
log "pulling $GRAFT_IMAGE"
# The attester (profile `attester`) is built here, not pulled.
compose pull --quiet graft-server graft-mirror

# Pre-flight (WI-2291): the configs must parse in the image about to run
# them, or a config can land ahead of the image that knows its fields and
# crash-loop the server. Both checks run in throwaway containers with no
# network and no secrets; nothing running is touched.
#
# graft-server loads and checks its config before it dispatches a command,
# so `admin` with no subcommand exits 78 on a bad config and 64 (usage) on a
# good one.
check_server_config() {
  local out rc=0
  out=$(docker run --rm --network none --user 10001:10001 \
    --entrypoint graft-server -v "$here/server.toml:/c/server.toml:ro" \
    "$GRAFT_IMAGE" --config /c/server.toml admin 2>&1) || rc=$?
  if [ "$rc" -ne 64 ]; then
    printf '%s\n' "$out" | tail -n 5 >&2
    die "pre-flight: deploy/server.toml does not load in $GRAFT_IMAGE (exit $rc); the running stack is unchanged"
  fi
}

# graft-mirror has no check command: `serve` parses and checks its config,
# then reads the token file. With no token mounted, reaching that read means
# the config passed. The data directory is a tmpfs, so the pre-flight makes
# nothing on the host.
check_mirror_config() {
  local out rc=0
  out=$(timeout 60 docker run --rm --network none --user 10001:10001 \
    --tmpfs /var/lib/graft/mirror:rw,uid=10001,size=8m \
    --entrypoint graft-mirror -v "$here/mirror-service.toml:/c/mirror-service.toml:ro" \
    "$GRAFT_IMAGE" serve --config /c/mirror-service.toml 2>&1) || rc=$?
  if ! printf '%s' "$out" | grep -q 'reading the token file'; then
    printf '%s\n' "$out" | tail -n 5 >&2
    die "pre-flight: deploy/mirror-service.toml does not load in $GRAFT_IMAGE (exit $rc); the running stack is unchanged"
  fi
}

log "pre-flight: checking server.toml and mirror-service.toml with $GRAFT_IMAGE"
check_server_config
check_mirror_config

# The last good deploy: image.env, server.toml and mirror-service.toml as they
# were when the stack last came up healthy. Kept under the secrets directory
# (root-owned, written like the secrets) so it survives a fresh checkout.
# They hold no secrets. A record made before mirror-service.toml existed lacks
# it; a rollback then leaves the checkout's copy in place.
last_good=last-good
have_last_good() {
  as_root "test -s /s/$last_good/image.env && test -s /s/$last_good/server.toml"
}

record_last_good() {
  tar -C "$here" -cf - image.env server.toml mirror-service.toml \
    | as_root "set -e
      rm -rf /s/.$last_good.tmp
      mkdir -m 0755 /s/.$last_good.tmp
      tar -C /s/.$last_good.tmp -xf -
      chmod 0644 /s/.$last_good.tmp/*
      rm -rf /s/$last_good
      mv /s/.$last_good.tmp /s/$last_good"
}

# GRAFT_CONFIG_HASH: a change to any config recreates the containers.
config_hash() {
  GRAFT_CONFIG_HASH=$(cat "$here/server.toml" "$here/mirror-service.toml" | sha256sum | cut -c1-16)
  export GRAFT_CONFIG_HASH
}

# Wait up to $ready_wait seconds for the server's /readyz.
wait_ready() {
  local deadline=$((SECONDS + ready_wait))
  until curl -fsS -o /dev/null "$base/readyz"; do
    [ "$SECONDS" -lt "$deadline" ] || return 1
    sleep 3
  done
}

# The new stack did not come up: put back the last good pin and configs in
# this checkout, bring that stack up, and fail the deploy either way.
rollback() {
  local why=$1
  log "error: $why"
  if ! have_last_good; then
    die "$why; no last good deploy is recorded under $secrets/$last_good, so nothing was rolled back"
  fi
  log "rolling back to the last good image pin and configs"
  local f
  for f in image.env server.toml mirror-service.toml; do
    as_root "test -s /s/$last_good/$f" || continue
    as_root "cat /s/$last_good/$f" > "$here/.$f.tmp"
    mv "$here/.$f.tmp" "$here/$f"
  done
  set -a
  # shellcheck source=/dev/null
  . "$here/image.env"
  set +a
  config_hash
  if compose up -d --wait --remove-orphans graft-server graft-mirror && wait_ready; then
    die "$why; rolled back to $GRAFT_IMAGE, which is up"
  fi
  die "$why; the rollback to $GRAFT_IMAGE did not come up either (see: docker logs graft-dev-server)"
}

if [ ! -e "$secrets/signing.key" ]; then
  log "bootstrap: creating $secrets/signing.key"
  as_root "head -c 32 /dev/urandom" | store_secret signing.key
fi

config_hash

log "starting graft-server"
compose up -d --wait graft-server \
  || rollback "graft-server did not become healthy on $GRAFT_IMAGE"
log "readiness: $base/readyz (waiting up to ${ready_wait}s)"
wait_ready || rollback "$base/readyz did not pass within ${ready_wait}s on $GRAFT_IMAGE"

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

# The mirror service's one token: kind `mirror`, scope `repo:*:mirror`, so it
# reaches every mirrored repository and nothing else. A service that was
# already running when its token was re-minted is restarted to read it.
mirror_token=mirror-service-token
restart_mirror=0
if due "$mirror_token" "mirror service token"; then
  [ -n "$(compose ps -q graft-mirror)" ] && restart_mirror=1
  compose exec -T graft-server graft-server admin token mint \
    --subject graft-mirror --kind mirror \
    --scope 'repo:*:mirror' --ttl "$ttl" \
    | store_secret "$mirror_token"
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

# The post-land checker's token. `land` on its line lands the reverts;
# `write` on the whole repository is what a revert needs, since Revert
# requires write on every path the bad land touched; `attest` posts the
# failures and interior passes; `read` follows the journal. Owned by uid
# 10001, the checker's user, which runs no checks. TTL 30 days, re-minted
# after GRAFT_TOKEN_RENEW_DAYS (20) by the next deploy.
post_land_mint=0
post_land_was_up=
if [ "$post_land" = 1 ]; then
  due post-land-token "post-land checker token" && post_land_mint=1
  post_land_was_up=$(compose ps -q graft-post-land)
  if [ "$post_land_mint" = 1 ]; then
    compose exec -T graft-server graft-server admin token mint \
      --subject graft-post-land-graft --kind service \
      --scope repo:graft:read --scope repo:graft:attest \
      --scope repo:graft:write --scope "repo:graft:land:$post_land_line" \
      --ttl "$ttl" \
      | store_secret post-land-token
  fi
  log "building the post-land checker image from $here/.."
  compose build graft-post-land
fi

log "starting the stack"
compose up -d --wait --remove-orphans \
  || rollback "the stack did not become healthy on $GRAFT_IMAGE"
if [ "$restart_mirror" = 1 ]; then
  # A new token for a service that was running: restart it so it reads it.
  compose up -d --wait --no-deps --force-recreate graft-mirror
fi
if [ "$attester_mint" = 1 ] && [ -n "$attester_was_up" ]; then
  compose up -d --wait --no-deps --force-recreate graft-attester
fi
if [ "$post_land_mint" = 1 ] && [ -n "$post_land_was_up" ]; then
  compose up -d --wait --no-deps --force-recreate graft-post-land
fi

log "health: $base/healthz"
curl -fsS -o /dev/null "$base/healthz" || rollback "$base/healthz failed on $GRAFT_IMAGE"
wait_ready || rollback "$base/readyz failed on $GRAFT_IMAGE"
record_last_good
log "recorded $GRAFT_IMAGE and its configs as the last good deploy"
log "stack is up"
