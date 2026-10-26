#!/usr/bin/env bash
# WI-2487: up.sh installs /etc/graft/github-token through as_root (mode 0600,
# owned by 10001:10001, like the other secrets), stages the mounted mirror
# configs through as_root, and still deploys every mirror, public and private,
# when the token is missing or empty. The token never appears on stdout or
# stderr.
#
# The real up.sh functions run here. Only as_root is replaced, by a fake root:
# secret files are kept mode 000 between calls, so a step that reads one as
# the runner user fails, and each call checks the modes it leaves behind.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d)
trap 'chmod -R u+rwX "$tmp" 2>/dev/null; rm -rf "$tmp"' EXIT

token='ghs_test_token_value_not_a_real_one'
fail() { echo "FAIL: $*" >&2; exit 1; }

# A fixture checkout: up.sh's own configs, registry and scripts.
fixture=$tmp/deploy
mkdir -p "$fixture"
cp "$root"/deploy/{up.sh,image.env,server.toml,mirrors.tsv,private-follow.sh} "$fixture/"
cp "$root"/deploy/mirror*.toml "$fixture/"

mirror_configs=()
private_configs=()
public_configs=()
while IFS='|' read -r svc _ _ _ _ cfg _ _ private; do
  [[ -n $svc && $svc != \#* ]] || continue
  mirror_configs+=("$cfg")
  if [ "${private:-0}" = 1 ]; then private_configs+=("$cfg"); else public_configs+=("$cfg"); fi
done <"$fixture/mirrors.tsv"
[ "${#private_configs[@]}" -ge 2 ] || fail "expected private mirrors (myaiagent, rdpapp) in mirrors.tsv"
[ "${#public_configs[@]}" -ge 1 ] || fail "expected public mirrors in mirrors.tsv"
printf '%s\n' "${private_configs[@]}" | grep -qx mirror-rdpapp.toml || fail "rdpapp is not a private mirror"
printf '%s\n' "${private_configs[@]}" | grep -qx mirror-myaiagent.toml || fail "myaiagent is not a private mirror"

# chown is recorded, not performed (the test is not root).
bin=$tmp/bin
mkdir -p "$bin"
cat >"$bin/chown" <<'SH'
#!/usr/bin/env bash
f=$(basename "$2"); f=${f#.}; f=${f%.tmp}
printf '%s\n' "$1" >"$FAKE_OWNERS/$f"
SH
chmod +x "$bin/chown"
export FAKE_OWNERS=$tmp/owners
mkdir -p "$FAKE_OWNERS"

lock() { find "$@" -maxdepth 1 -type f -perm 0600 -exec chmod 000 {} + 2>/dev/null || true; }
unlock() { find "$@" -maxdepth 1 -type f -perm 0000 -exec chmod 0600 {} + 2>/dev/null || true; }

# The fake root: unlock, run the snippet with /s (and any as_root_mounts
# target) mapped to the host paths, check modes, then lock the secrets again.
as_root() {
  local snippet=$1 m src="" target="" rc=0
  for m in "${as_root_mounts[@]}"; do
    case $m in type=bind,*)
      src=${m#*source=}; src=${src%%,*}
      target=${m#*target=}; target=${target%%,*} ;;
    esac
  done
  if [ -n "$target" ]; then
    chmod 0600 "$src" 2>/dev/null || true
    snippet=${snippet//$target/$src}
  fi
  snippet=${snippet//\/s\//$secrets/}
  unlock "$secrets"
  PATH="$bin:$PATH" sh -c "$snippet" || rc=$?
  if [ -e "$secrets/github-token" ]; then
    [ "$(stat -c %a "$secrets/github-token")" = 600 ] || fail "github-token mode is $(stat -c %a "$secrets/github-token")"
    [ "$(cat "$FAKE_OWNERS/github-token" 2>/dev/null)" = 10001:10001 ] || fail "github-token not owned by 10001:10001"
  fi
  if [ -d "$secrets/mirror-configs" ]; then
    [ "$(stat -c %a "$secrets/mirror-configs")" = 755 ] || fail "mirror-configs dir mode"
    for m in "$secrets"/mirror-configs/*; do
      [ "$(stat -c %a "$m")" = 644 ] || fail "$m mode"
    done
  fi
  lock "$secrets"
  [ -z "$src" ] || chmod 000 "$src"
  return "$rc"
}

log() { printf 'up.sh: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }
for f in install_github_token strip_github_table cleanup_stage prepare_mounted_configs \
  install_mounted_configs record_last_good have_last_good config_hash rollback; do
  body=$(sed -n "/^$f()/,/^}/p" "$fixture/up.sh")
  [ -n "$body" ] || fail "up.sh has no $f"
  eval "$body"
done
sed -n '/^install_github_token()/,/^}/p' "$fixture/up.sh" | grep -q 'HOMELAB_VOGT_GITHUB_TOKEN' \
  && fail 'install_github_token fetches from Infisical'
# shellcheck disable=SC2034 # read by the eval-ed up.sh functions
mounted_configs=mirror-configs
# shellcheck disable=SC2034

last_good=last-good
as_root_mounts=()

no_token_in() {
  if grep -F -- "$token" "$@"; then fail "token appeared in output"; fi
}
read_secret() { chmod 0600 "$1"; cat "$1"; chmod 000 "$1"; }

# A fresh fake /etc/graft. $1 is the github-token content, or "-" for none.
new_host() {
  secrets=$tmp/host-$1-$RANDOM
  mkdir -p "$secrets"
  rm -f "$FAKE_OWNERS"/*
  github_ready=9
  here=$fixture
  stage=
}

deploy_step() {
  out=$tmp/out err=$tmp/err
  install_github_token "$1" >"$out" 2>"$err"
  prepare_mounted_configs >>"$out" 2>>"$err"
}

# --- present, placed at the secret path itself (the graft-dev layout) -------
new_host present
printf '%s\n' "$token" >"$secrets/github-token"
chmod 000 "$secrets/github-token"      # the runner cannot read it
if cat "$secrets/github-token" 2>/dev/null; then echo "(test runs as root; read checks are weaker)" >&2; fi
deploy_step "$secrets/github-token"
[ "$github_ready" = 1 ] || fail "token present but github_ready=$github_ready"
[ "$(read_secret "$secrets/github-token")" = "$token" ] || fail "token content changed"
no_token_in "$out" "$err"
grep -q 'mode 0600, uid 10001, not printed' "$err" || fail "no install log line"
[ "$GRAFT_MIRROR_CONFIG_DIR" = "$secrets/mirror-configs" ] || fail "GRAFT_MIRROR_CONFIG_DIR=$GRAFT_MIRROR_CONFIG_DIR"
for cfg in "${mirror_configs[@]}"; do
  cmp -s "$fixture/$cfg" "$secrets/mirror-configs/$cfg" || fail "$cfg mounted copy differs from checkout"
  grep -q '^\[github\]' "$secrets/mirror-configs/$cfg" || fail "$cfg lacks [github] with a token"
  grep -q '^token_file = "/run/secrets/github-token"' "$secrets/mirror-configs/$cfg" || fail "$cfg token_file"
done
present_stage=$stage
[ -d "$present_stage" ] || fail "stage missing"
# Record this deploy as last good: the mounted copies are what is recorded.
record_last_good >/dev/null 2>&1
for cfg in "${mirror_configs[@]}"; do
  cmp -s "$secrets/mirror-configs/$cfg" "$secrets/last-good/$cfg" || fail "last-good $cfg is not the mounted copy"
done
cleanup_stage
[ ! -e "$present_stage" ] || fail "stage not cleaned up"
echo "ok: token present at the secret path: 0600, 10001:10001, runner never reads it, every mirror mounts [github]"

# --- present, at a separate source path (mounted read-only into as_root) ----
new_host srcpath
src=$tmp/placed-github-token
printf '%s\n' "$token" >"$src"
chmod 000 "$src"
deploy_step "$src"
[ "$github_ready" = 1 ] || fail "separate source: github_ready=$github_ready"
[ "$(read_secret "$secrets/github-token")" = "$token" ] || fail "separate source: content"
no_token_in "$out" "$err"
cleanup_stage
echo "ok: token at a separate source path copied by as_root, 0600"

# --- missing and empty: one warning, deploy continues, [github] omitted -----
for kind in missing empty blank; do
  new_host "$kind"
  case $kind in
    empty) : >"$secrets/github-token" ;;
    blank) printf '\n \n' >"$secrets/github-token" ;;
  esac
  [ ! -e "$secrets/github-token" ] || chmod 000 "$secrets/github-token"
  deploy_step "$secrets/github-token"
  [ "$github_ready" = 0 ] || fail "$kind: github_ready=$github_ready"
  [ -f "$secrets/github-token" ] || fail "$kind: no mount file"
  [ ! -s "$secrets/github-token" ] || fail "$kind: mount file not empty"
  [ "$(grep -c 'warning:' "$err")" = 1 ] || fail "$kind: expected exactly one warning"
  no_token_in "$out" "$err"
  for cfg in "${mirror_configs[@]}"; do
    m=$secrets/mirror-configs/$cfg
    if grep -q '^\[github\]' "$m"; then fail "$kind: $cfg still has [github]"; fi
    grep -q '^\[\[lines\]\]' "$m" || fail "$kind: $cfg lost [[lines]]"
    grep -q '^repo = ' "$m" || fail "$kind: $cfg lost its top level"
  done
  cleanup_stage
done
echo "ok: token missing/empty: one warning, deploy step succeeds, [github] omitted for public and private mirrors"

# --- private mirrors start with an empty token ------------------------------
pf=$tmp/private-follow.sh
sed "s#/run/secrets/github-token#$tmp/empty-token#" "$fixture/private-follow.sh" >"$pf"
: >"$tmp/empty-token"
cat >"$bin/graft-mirror" <<'SH'
#!/usr/bin/env bash
printf 'graft-mirror %s\n' "$*"
printf 'header=%s\n' "${GIT_CONFIG_VALUE_0:-none}"
SH
chmod +x "$bin/graft-mirror"
pout=$(PATH="$bin:$PATH" bash "$pf" 2>"$tmp/perr") || fail "private-follow.sh exits with an empty token"
printf '%s\n' "$pout" | grep -q '^graft-mirror follow --config /etc/graft/deploy/mirror.toml$' || fail "private-follow did not start follow"
printf '%s\n' "$pout" | grep -q '^header=none$' || fail "private-follow set a header without a token"
grep -q 'warning' "$tmp/perr" || fail "private-follow did not warn"
printf '%s\n' "$token" >"$tmp/empty-token"
pout=$(PATH="$bin:$PATH" bash "$pf" 2>"$tmp/perr")
printf '%s\n' "$pout" | grep -q '^header=Authorization: Basic ' || fail "private-follow lost its header with a token"
grep -F -- "$token" "$tmp/perr" && fail "private-follow printed the token"
echo "ok: private mirrors start with an empty token and authenticate with one"

# --- rollback restores exactly the mounted configs of the last good deploy --
new_host rollback
printf '%s\n' "$token" >"$secrets/github-token"
chmod 000 "$secrets/github-token"
deploy_step "$secrets/github-token"
record_last_good >/dev/null 2>&1
cleanup_stage
chmod 0600 "$secrets/github-token"
: >"$secrets/github-token"  # the token goes away; the next deploy strips [github]
chmod 000 "$secrets/github-token"
deploy_step "$secrets/github-token"
grep -q '^\[github\]' "$secrets/mirror-configs/mirror.toml" && fail "second deploy kept [github]"
compose() { return 0; }
wait_ready() { return 0; }
( rollback "test failure" ) >"$tmp/rb.out" 2>"$tmp/rb.err" && fail "rollback returned success"
grep -q 'rolled back to' "$tmp/rb.err" || { cat "$tmp/rb.err" >&2; fail "rollback did not complete"; }
for cfg in "${mirror_configs[@]}"; do
  cmp -s "$secrets/last-good/$cfg" "$secrets/mirror-configs/$cfg" || fail "rollback: mounted $cfg is not the last good copy"
done
no_token_in "$tmp/rb.out" "$tmp/rb.err"
cleanup_stage
echo "ok: rollback restores the last good mounted configs"
