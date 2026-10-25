#!/usr/bin/env bash
# Follow a GitHub repository on graft-dev and create an integration line.
# Session tokens are minted separately, one per session, and are never stored
# for sessions to share.
#
# Usage: deploy/onboard-repo.sh [--dry-run] OWNER/REPO BASE [LINE]
#        deploy/onboard-repo.sh --mint-session OWNER/REPO SESSION [--work-item WI-N] [--ttl SECS]
set -euo pipefail
# Session tokens stay 0600. Generated mirror configs are 0644 so container
# uid 10001 can read them; tighten again around the session-token write.
umask 022

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
registry=$here/mirrors.tsv
dry_run=0

usage() {
  sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}
log() { printf 'onboard-repo: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

mint_session=
work_item=
ttl=86400
if [[ ${1:-} == --dry-run ]]; then dry_run=1; shift; fi
if [[ ${1:-} == --mint-session ]]; then mint_session=1; shift; fi
[[ ${1:-} != -h && ${1:-} != --help ]] || usage
if [[ -n $mint_session ]]; then
  [[ $dry_run == 0 ]] || die '--mint-session cannot be combined with --dry-run'
  [[ $# -ge 2 ]] || usage
  owner_repo=$1
  session=$2
  shift 2
  while [[ $# -gt 0 ]]; do
    case $1 in
      --work-item) [[ $# -ge 2 ]] || usage; work_item=$2; shift 2 ;;
      --ttl) [[ $# -ge 2 ]] || usage; ttl=$2; shift 2 ;;
      *) usage ;;
    esac
  done
  [[ $session =~ ^ses_[A-Za-z0-9]+$ ]] || die '--session must look like ses_…'
  [[ -z $work_item || $work_item =~ ^WI-[0-9]+$ ]] || die '--work-item must look like WI-123'
  [[ $ttl =~ ^[1-9][0-9]*$ ]] || die '--ttl takes seconds'
  base=main
  line=trial-1
else
  [[ $# -ge 2 && $# -le 3 ]] || usage
  owner_repo=$1
  base=$2
  line=${3:-trial-1}
fi

[[ $owner_repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die 'repo must be OWNER/REPO'
[[ $base =~ ^[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$ ]] || die 'invalid base branch'
[[ $line =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || die 'invalid line'
[[ $line != "$base" ]] || die 'line must differ from the followed base branch'

github_repo=${owner_repo#*/}
repo=$(printf '%s' "$github_repo" | tr '[:upper:].' '[:lower:]_')
[[ $repo =~ ^[a-z0-9][a-z0-9_-]{0,63}$ ]] || die 'repository name cannot map to a graft repository id'
suffix=$(printf '%s' "$repo" | tr '_' '-')
service=graft-mirror-$suffix
token_file=mirror-token-$suffix
config=mirror-$suffix.toml
existing=$(awk -F'|' -v upstream="$owner_repo" '$1 !~ /^#/ && $3 == upstream {print; exit}' "$registry")
if [[ -n $existing ]]; then
  IFS='|' read -r service repo _ registered_base token_file config port _source <<<"$existing"
  suffix=${service#graft-mirror-}
  [[ $registered_base == "$base" ]] || die "$owner_repo is registered to follow $registered_base, not $base; refusing to keep the old base silently"
  log "$owner_repo is already registered as $repo following $base"
else
  awk -F'|' -v repo="$repo" -v service="$service" '$1 == service || $2 == repo {found=1} END {exit !found}' "$registry" \
    && die "service or graft repository already exists for $repo"
  port=$(( $(awk -F'|' '$1 !~ /^#/ {if ($7 > max) max=$7} END {print max+0}' "$registry") + 1 ))
fi

if [[ -n $mint_session ]]; then
  [[ -n $existing ]] || die "$owner_repo is not registered; onboard it before minting a session token"
  command -v docker >/dev/null || die 'docker is required; run this command in the graft-dev checkout'
  set -a
  # shellcheck source=/dev/null
  . "$here/image.env"
  set +a
  extra=()
  [[ -z $work_item ]] || extra+=(--work-item "$work_item")
  out=${GRAFT_TOKEN_DIR:-$HOME/.config/graft}/$repo-$session.token
  umask 077
  mkdir -p "$(dirname "$out")"
  chmod 0700 "$(dirname "$out")"
  tmp=$out.new
  compose=(docker compose --env-file "$here/image.env" -f "$here/compose.yml" -f "$here/compose.onboarded.yml")
  if ! "${compose[@]}" exec -T graft-server graft-server admin token mint \
      --subject "$session" --kind agent --session "$session" \
      --scope "repo:$repo:read" --scope "repo:$repo:write" \
      --scope "repo:$repo:drafts" --scope "repo:$repo:land:$line" \
      --ttl "$ttl" "${extra[@]}" >"$tmp"; then
    rm -f "$tmp"
    die "session token mint failed; nothing stored"
  fi
  [[ -s $tmp ]] || { rm -f "$tmp"; die 'session token was empty'; }
  chmod 0600 "$tmp"
  mv "$tmp" "$out"
  log "wrote a session-bound agent token for $session ($(wc -c <"$out") bytes, not printed)"
  exit 0
fi

cat <<PLAN
Repository:       https://github.com/$owner_repo.git
Graft repository: $repo
Followed base:    $base
Integration line:$line
Mirror service:  $service
Metrics port:    $port
Agent tokens:     kind agent, one per session, never stored for sharing
PLAN

cat <<RULESET

Required GitHub ruleset for $owner_repo ($base):
- Require a pull request before merging.
- Require the repository's normal required status checks.
- Allow rebase merges and disable merge commits for the graft batch path.
- Add the operator used by land-pr as a bypass actor (always allow).
- Batch PRs named graft/batch-* are reviewed, then land-pr rebase-merges them with --admin (D28).
RULESET

if [[ $dry_run == 1 ]]; then
  cat <<DRY

Dry run only; would:
1. Add $registry, $here/$config and regenerate compose.onboarded.yml when absent.
2. Run $here/up.sh (the checked deploy and per-repository refs wait).
3. Create line $line from $base, retrying while the first import has no ref.
4. Leave session minting to a later call:
   deploy/onboard-repo.sh --mint-session $owner_repo ses_… [--work-item WI-N]
   That mints one --kind agent token with --session, scopes read/write/drafts/land:$line,
   and writes it to a mode-0600 file. It is never stored for another session to share.
DRY
  exit 0
fi

command -v docker >/dev/null || die 'docker is required; run this command in the graft-dev checkout'
set -a
# shellcheck source=/dev/null
. "$here/image.env"
set +a

if [[ -z $existing ]]; then
  # Public mirrors stay unauthenticated. A private upstream gets a host file
  # reference only: the container reads it into git http.extraHeader. The
  # token is never written into this config or into process arguments.
  private=0
  ls_remote_error=$(mktemp)
  if git ls-remote --heads "https://github.com/$owner_repo.git" >/dev/null 2>"$ls_remote_error"; then
    :
  elif grep -Eqi 'authentication failed|could not read username|terminal prompts disabled|repository not found|http basic: access denied' "$ls_remote_error"; then
    private=1
  else
    rm -f "$ls_remote_error"
    die "could not check whether $owner_repo is private; GitHub or the network is unavailable"
  fi
  rm -f "$ls_remote_error"
  github_section=
  if [[ $private == 1 ]]; then
    github_section=$(cat <<'GH'

[github]
owner = "OWNER"
repo = "REPO"
token_file = "/run/secrets/github-token"
GH
)
    github_section=${github_section/OWNER/${owner_repo%%/*}}
    github_section=${github_section/REPO/${owner_repo#*/}}
  fi
  cat >"$here/$config" <<CONFIG
# Generated by deploy/onboard-repo.sh. Nothing here is secret.
server = "http://graft-server:7878"
repo = "$repo"
upstream = "https://github.com/$owner_repo.git"
mirror_dir = "/var/lib/graft/mirror/$repo.git"
db = "/var/lib/graft/mirror/$repo.sqlite"
token_file = "/run/secrets/graft-mirror-token"
poll_interval_secs = 15
metrics_addr = "0.0.0.0:9465"
json_logs = true
$github_section
[[lines]]
branch = "$base"
line = "$base"
CONFIG
  chmod 0644 "$here/$config"
  printf '%s|%s|%s|%s|%s|%s|%s|generated|%s\n' \
    "$service" "$repo" "$owner_repo" "$base" "$token_file" "$config" "$port" "$private" >>"$registry"
  "$here/render-onboarded-compose.sh"
fi

"$here/up.sh"

compose=(docker compose --env-file "$here/image.env" -f "$here/compose.yml" -f "$here/compose.onboarded.yml")
work=$(mktemp -d "${TMPDIR:-/tmp}/graft-onboard.XXXXXX")
cleanup() {
  if [[ -s ${work:-}/admin.token ]]; then
    "${compose[@]}" exec -T graft-server graft-server admin token revoke <"$work/admin.token" \
      >/dev/null 2>&1 || log 'warning: temporary admin token was not revoked'
  fi
  rm -rf "$work"
}
trap cleanup EXIT
"${compose[@]}" exec -T graft-server graft-server admin token mint \
  --subject "onboard-$repo" --kind service --scope "repo:$repo:admin" --ttl 3600 >"$work/admin.token"
[[ -s $work/admin.token ]] || die 'temporary admin token was empty'

attempts=${GRAFT_LINE_ATTEMPTS:-60}
attempt=1
while :; do
if ! docker run --rm --user 0:0 --network graft-dev \
    --entrypoint sh -v "$work:/w" "${GRAFT_IMAGE:?deploy/image.env did not set GRAFT_IMAGE}" -c '
      set -eu
      repo=$1; base=$2; line=$3
      g() { graft --endpoint http://graft-server:7878 --token-file /w/admin.token --repo "$repo" "$@"; }
      if g log --at "$line" --limit 1 >/dev/null 2>/w/error; then exit 0; fi
      grep -qi "not.found" /w/error || { cat /w/error >&2; exit 1; }
      if ! g log --at "$base" --limit 1 >/dev/null 2>/w/error; then
        grep -qi "not.found" /w/error && exit 75
        cat /w/error >&2
        exit 1
      fi
      g line create "$line" --from "$base" >/dev/null
    ' sh "$repo" "$base" "$line"; then
  rc=0
else
  rc=$?
fi
  if [[ $rc == 0 ]]; then
    break
  elif [[ $rc == 75 && $attempt -lt $attempts ]]; then
    log "base $base has no ref yet; waiting for the first import ($attempt/$attempts)"
    sleep "${GRAFT_LINE_POLL:-10}"
    attempt=$((attempt + 1))
  else
    die "could not create line $line from $base"
  fi
done
log "line $line exists and was not moved"

cat <<DONE

Onboarding complete. Mint one session-bound token when a session starts:
deploy/onboard-repo.sh --mint-session $owner_repo ses_… [--work-item WI-N]
The token is kind agent with --session and is never stored for another session.
DONE
