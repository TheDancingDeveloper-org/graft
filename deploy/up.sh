#!/usr/bin/env bash
# Deploy the graft-dev stack (deploy/compose.yml) on this host. Idempotent: run
# it by hand or from .github/workflows/deploy-dev.yml; a second run with
# nothing changed leaves the containers alone.
#
#   1. Bootstrap secrets under /etc/graft, if missing: the server signing key,
#      and (after the server is up) the mirror's service token. Both are
#      written by a throwaway root container from the pinned image, mode 0600,
#      owned by uid 10001. Neither is ever printed. github-token is copied
#      from /etc/graft/github-token when that file was placed on the VM. A
#      missing or empty source is a warning; the deploy continues and the
#      mirrors start without [github].
#   2. Pull the pinned image and pre-flight: check deploy/server.toml and
#      every deploy/mirror*.toml with that image's own binaries. A config the image
#      can't parse stops the deploy here, with the running stack untouched
#      (WI-2291).
#   3. `compose up -d --wait` and wait for /readyz. If the new stack does not
#      come up, put back the last good image pin and configs (kept under
#      $GRAFT_SECRETS_DIR/last-good by the last successful deploy), bring that
#      stack up and exit non-zero. A good deploy records itself as last good.
#   4. Create the repositories `rusttorrent`, `graft-dev`, `fluent2-egui` and
#      `vogt` (a retry of an existing repo is a success), mint each mirror's
#      token if absent or due for renewal.
#   5. Health-check /healthz, then the /v1 facade: the refs of each followed
#      repository (rusttorrent, fluent2-egui, vogt).
#
# With GRAFT_ATTESTER=1 (WI-2276; off by default) it also turns on the compose
# profile `attester`: it mints the attester's token (`read` and `attest` on
# `rusttorrent`, whose line `trial-1` it attests (decision D21); 0600, owned
# by root so the checks the attester runs as uid 10001 can't read it), builds
# the attester image from this checkout and starts it.
#
# Needs: docker with compose v2, curl, and membership of the docker group.
# Non-secret host settings (GRAFT_BIND_IP, ports) may go in
# /etc/graft/deploy.env. Environment overrides:
#   GRAFT_SECRETS_DIR       secrets directory (default /etc/graft)
#   GRAFT_MIRROR_CONFIG_DIR directory of the mirror configs compose mounts
#                           (set by this script; default is deploy/)
#   GRAFT_REMINT=1          mint a new mirror token even if the current one is fresh
#   GRAFT_TOKEN_RENEW_DAYS  re-mint a token older than this (default 20; TTL is 30)
#   GRAFT_REFS_WAIT         seconds each followed repo waits for its first ref
#                           (default 600). A repo still in its first import at
#                           its own deadline is a warning, not a failed deploy;
#                           an error from the refs endpoint is logged and fails
#                           the deploy.
#   GRAFT_ATTESTER=1        run the trial attester (profile `attester`)
#   GRAFT_ATTESTER_REPO     the repository it attests (default rusttorrent)
#   GRAFT_ATTESTER_LINE     the line whose drafts it attests (default trial-1)
#   GRAFT_READY_WAIT        seconds to wait for /readyz after a swap (default 120)
#
# Exit status: 0 deployed; 1 failed. A failed pre-flight or a rollback says so
# on stderr and, under GitHub Actions, as an ::error:: annotation.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
secrets=${GRAFT_SECRETS_DIR:-/etc/graft}
registry=$here/mirrors.tsv
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

# Every repository the stack serves; creating one that exists is a no-op.
repos=(graft-dev)
# One registry row per followed repository. The first follow is its import.
mirrors=()
while IFS='|' read -r svc mrepo _owner_repo _base tok cfg _port _source; do
  [[ -n $svc && $svc != \#* ]] || continue
  repos+=("$mrepo")
  mirrors+=("$svc $mrepo $tok $cfg")
done <"$registry"
mirror_services=()
mirror_configs=()
for m in "${mirrors[@]}"; do
  read -r svc _ _ cfg <<<"$m"
  mirror_services+=("$svc")
  mirror_configs+=("$cfg")
done
ttl=2592000
attester=${GRAFT_ATTESTER:-0}
attester_repo=${GRAFT_ATTESTER_REPO:-rusttorrent}
export GRAFT_ATTESTER_REPO=$attester_repo
if [ "$attester" = 1 ]; then
  export COMPOSE_PROFILES=attester
  # Created if it is not one the stack already serves (the default is).
  case " ${repos[*]} " in
    *" $attester_repo "*) ;;
    *) repos+=("$attester_repo") ;;
  esac
fi
renew_days=${GRAFT_TOKEN_RENEW_DAYS:-20}
refs_wait=${GRAFT_REFS_WAIT:-600}
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
  docker compose --env-file "$here/image.env" -f "$here/compose.yml" \
    -f "$here/compose.onboarded.yml" "$@"
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

# GitHub credential for every mirror's [github] token_file (WI-2487).
# Placed once on the VM at /etc/graft/github-token, outside this deploy.
# Copied into the secrets directory, mode 0600. The value is never printed
# or placed in argv or compose env. A missing or empty source does not fail
# the deploy: the mount points at an empty file and the mounted configs omit
# [github], so graft-mirror starts. pr-batch then fails with
# "pr-batch opens PRs: configure [github]" until the file is placed.
install_github_token() {
  local src=${1:-/etc/graft/github-token} staged
  github_ready=0
  if [ ! -s "$src" ]; then
    log "warning: $src is missing or empty; mirrors will start without [github] and pr-batch cannot open pull requests until that file is placed"
    as_root "set -e; : > /s/github-token; chmod 0600 /s/github-token; chown 10001:10001 /s/github-token 2>/dev/null || true"
    return 0
  fi
  # Stage first. The source may be the secrets file this is about to replace,
  # and the token must not be printed or placed in argv.
  staged=$(mktemp)
  cp -- "$src" "$staged"
  if [ ! -s "$staged" ]; then
    rm -f "$staged"
    log "warning: $src is missing or empty; mirrors will start without [github] and pr-batch cannot open pull requests until that file is placed"
    as_root "set -e; : > /s/github-token; chmod 0600 /s/github-token; chown 10001:10001 /s/github-token 2>/dev/null || true"
    return 0
  fi
  store_secret github-token <"$staged"
  rm -f "$staged"
  github_ready=1
  log "copied github-token ($(as_root "wc -c < /s/github-token") bytes, mode 0600, not printed)"
}

# Drop a trailing [github] table. [[lines]] stays.
strip_github_table() {
  awk '
    /^\[github\]/ { skip=1; next }
    /^\[/ { skip=0 }
    !skip { print }
  '
}

# Configs compose mounts. With a token they are the checkout copies. Without
# one they omit [github]. GRAFT_MIRROR_CONFIG_DIR points compose at them.
prepare_mounted_configs() {
  local cfg dir
  dir=$(mktemp -d "$secrets/mirror-configs.XXXXXX")
  for cfg in "${mirror_configs[@]}"; do
    if [ "$github_ready" = 1 ]; then
      cp -- "$here/$cfg" "$dir/$cfg"
    else
      strip_github_table < "$here/$cfg" > "$dir/$cfg"
    fi
    chmod 0644 "$dir/$cfg"
  done
  # mktemp is 0700; compose (and the pre-flight user) must be able to read it.
  chmod 0755 "$dir"
  export GRAFT_MIRROR_CONFIG_DIR=$dir
}

install_github_token /etc/graft/github-token
prepare_mounted_configs

log "pulling $GRAFT_IMAGE"
# The attester (profile `attester`) is built here, not pulled.
compose pull --quiet graft-server "${mirror_services[@]}"

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

# graft-mirror has no check command: `follow` parses and checks its config,
# then reads the token file. With no token mounted, reaching that read means
# the config passed.
check_mirror_config() {
  local cfg=$1 out rc=0 dir=${GRAFT_MIRROR_CONFIG_DIR:-$here}
  out=$(timeout 60 docker run --rm --network none --user 10001:10001 \
    --entrypoint graft-mirror -v "$dir/$cfg:/c/mirror.toml:ro" \
    "$GRAFT_IMAGE" follow --config /c/mirror.toml 2>&1) || rc=$?
  if ! printf '%s' "$out" | grep -q 'reading the token file'; then
    printf '%s\n' "$out" | tail -n 5 >&2
    die "pre-flight: deploy/$cfg does not load in $GRAFT_IMAGE (exit $rc); the running stack is unchanged"
  fi
}

log "pre-flight: checking server.toml and ${mirror_configs[*]} with $GRAFT_IMAGE"
check_server_config
for cfg in "${mirror_configs[@]}"; do
  check_mirror_config "$cfg"
done

# The last good deploy: image.env, server.toml and the mirror configs as they were
# when the stack last came up healthy. Kept under the secrets directory
# (root-owned, written like the secrets) so it survives a fresh checkout.
# They hold no secrets. A record made before a mirror config existed lacks
# that file; a rollback then leaves the checkout's copy in place.
last_good=last-good
have_last_good() {
  as_root "test -s /s/$last_good/image.env && test -s /s/$last_good/server.toml && test -s /s/$last_good/mirror.toml"
}

record_last_good() {
  tar -C "$here" -cf - image.env server.toml "${mirror_configs[@]}" \
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
  GRAFT_CONFIG_HASH=$( (cd "$here" && cat server.toml "${mirror_configs[@]}") | sha256sum | cut -c1-16)
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
  for f in image.env server.toml "${mirror_configs[@]}"; do
    as_root "test -s /s/$last_good/$f" || continue
    as_root "cat /s/$last_good/$f" > "$here/.$f.tmp"
    mv "$here/.$f.tmp" "$here/$f"
  done
  set -a
  # shellcheck source=/dev/null
  . "$here/image.env"
  set +a
  config_hash
  if compose up -d --wait --remove-orphans graft-server "${mirror_services[@]}" && wait_ready; then
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

# Each mirror's token, scoped to its own repository. A mirror that was
# already running when its token was re-minted is restarted to read it.
restart_mirrors=()
for m in "${mirrors[@]}"; do
  read -r svc mrepo tok _ <<<"$m"
  due "$tok" "$svc token" || continue
  [ -n "$(compose ps -q "$svc")" ] && restart_mirrors+=("$svc")
  compose exec -T graft-server graft-server admin token mint \
    --subject "graft-mirror-$mrepo" --kind service \
    --scope "repo:$mrepo:admin" --ttl "$ttl" \
    | store_secret "$tok"
done

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
compose up -d --wait --remove-orphans \
  || rollback "the stack did not become healthy on $GRAFT_IMAGE"
if [ "${#restart_mirrors[@]}" -gt 0 ]; then
  # A new token for a mirror that was running: restart it so it reads it.
  compose up -d --wait --no-deps --force-recreate "${restart_mirrors[@]}"
fi
if [ "$attester_mint" = 1 ] && [ -n "$attester_was_up" ]; then
  compose up -d --wait --no-deps --force-recreate graft-attester
fi

log "health: $base/healthz"
curl -fsS -o /dev/null "$base/healthz" || rollback "$base/healthz failed on $GRAFT_IMAGE"
wait_ready || rollback "$base/readyz failed on $GRAFT_IMAGE"
record_last_good
log "recorded $GRAFT_IMAGE and its configs as the last good deploy"

# The /v1 facade, authenticated with the mirror token. The token goes to curl
# on stdin (-H @-), never in arguments, and is never printed.
# Each repository is asked with its own mirror's token.
# Prints the body on success. On a transport or HTTP error, prints curl's
# message to stderr and returns curl's status, so a caller can tell "no refs
# yet" (success, empty list) from an endpoint that is failing.
refs() {
  as_root "cat /s/$2" \
    | sed 's/^/Authorization: Bearer /' \
    | curl -fsS -H @- "$base/v1/repos/$1/refs?limit=100"
}

# Wait for one followed repository's first ref. The deadline is this repo's
# own $refs_wait seconds, not a budget shared with the repos already checked:
# a first import of a later repo (vogt is large) gets the full wait. A repo
# that answers with an empty ref list is still importing. When the deadline
# passes with no ref, that is "no ref after import" and fails the deploy,
# distinct from the importing log. An error from the endpoint also fails.
wait_for_refs() {
  local svc=$1 mrepo=$2 tok=$3
  local deadline=$((SECONDS + refs_wait)) err rc
  log "health: $base/v1/repos/$mrepo/refs (waiting up to ${refs_wait}s for a ref)"
  while :; do
    err=$(mktemp)
    if body=$(refs "$mrepo" "$tok" 2>"$err"); then
      rc=0
    else
      rc=$?
    fi
    if [ "$rc" -eq 0 ]; then
      count=$(printf '%s' "$body" | { grep -o '"name"' || true; } | wc -l)
      rm -f "$err"
      if [ "$count" -gt 0 ]; then
        log "ok: $mrepo has $count ref(s)"
        return 0
      fi
      log "$mrepo is importing (no refs yet)"
    else
      log "error: /v1 refs for $mrepo failed (curl exit $rc): $(tr '\n' ' ' <"$err")"
      rm -f "$err"
      die "$mrepo refs endpoint failed; not treating an endpoint error as a slow import (see: docker logs graft-dev-${svc#graft-})"
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      log "error: $mrepo has no ref after import (${refs_wait}s); this is not the importing state (see: docker logs graft-dev-${svc#graft-})"
      die "$mrepo has no ref after import"
    fi
    sleep "${GRAFT_REFS_POLL:-10}"
  done
}

for m in "${mirrors[@]}"; do
  read -r svc mrepo tok _ <<<"$m"
  wait_for_refs "$svc" "$mrepo" "$tok"
done
log "stack is up"
