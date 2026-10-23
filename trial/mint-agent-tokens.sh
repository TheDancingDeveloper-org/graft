#!/usr/bin/env bash
# Mint N agent tokens for the rusttorrent repository on graft-dev and
# store them on THIS machine as ~/.config/graft/trial-<i>.token (mode 0600).
#
#   trial/mint-agent-tokens.sh [--session ses_…] [--work-item WI-…] [--ttl SECS] N
#
# Each token is kind agent, scopes repo:rusttorrent:read, :write and
# :land:trial-1 (never main), valid for one day by default. Minting runs on the VM through
# `tailscale ssh sprooty@graft-dev`; the token travels back on the ssh stdout
# straight into the file. It is never printed, never in argv, never in a
# variable. Re-running replaces the files with fresh tokens.
#
#   GRAFT_TRIAL_HOST    ssh target (default sprooty@graft-dev)
#   GRAFT_TRIAL_DIR     token directory (default ~/.config/graft)
set -euo pipefail

host=${GRAFT_TRIAL_HOST:-sprooty@graft-dev}
dir=${GRAFT_TRIAL_DIR:-$HOME/.config/graft}
repo=rusttorrent
line=trial-1
ttl=86400
session=
work_item=

log() { printf 'mint-agent-tokens: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }
usage() { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

n=
while [ $# -gt 0 ]; do
  case $1 in
    --session) [ $# -ge 2 ] || usage; session=$2; shift 2 ;;
    --work-item) [ $# -ge 2 ] || usage; work_item=$2; shift 2 ;;
    --ttl) [ $# -ge 2 ] || usage; ttl=$2; shift 2 ;;
    -h|--help) usage ;;
    -*) usage ;;
    *) [ -z "$n" ] || usage; n=$1; shift ;;
  esac
done
[ -n "$n" ] || usage
# These are interpolated into the remote command line: accept only safe shapes.
[[ $n =~ ^[1-9][0-9]*$ ]] && [ "$n" -le 64 ] || die "N must be 1..64"
[[ $ttl =~ ^[1-9][0-9]*$ ]] || die "--ttl takes seconds"
[ -z "$session" ] || [[ $session =~ ^ses_[A-Za-z0-9]+$ ]] || die "--session must look like ses_…"
[ -z "$work_item" ] || [[ $work_item =~ ^WI-[0-9]+$ ]] || die "--work-item must look like WI-123"

command -v tailscale >/dev/null || die "tailscale is required"

extra=
[ -z "$session" ] || extra="$extra --session $session"
[ -z "$work_item" ] || extra="$extra --work-item $work_item"

umask 077
mkdir -p "$dir"
chmod 0700 "$dir"

for i in $(seq 1 "$n"); do
  out=$dir/trial-$i.token
  tmp=$out.new
  rm -f "$tmp"
  # shellcheck disable=SC2086 # $extra is validated words, split on purpose
  if ! tailscale ssh "$host" docker exec graft-dev-server graft-server admin token mint \
      --subject "trial-agent-$i" --kind agent \
      --scope "repo:$repo:read" --scope "repo:$repo:write" --scope "repo:$repo:land:$line" \
      --ttl "$ttl" $extra >"$tmp"; then
    rm -f "$tmp"
    die "mint $i failed (nothing written for it)"
  fi
  [ -s "$tmp" ] || { rm -f "$tmp"; die "mint $i returned an empty token"; }
  chmod 0600 "$tmp"
  mv -f "$tmp" "$out"
  log "wrote $out ($(wc -c <"$out") bytes)"
done
log "done: $n token(s) in $dir, ttl ${ttl}s"
