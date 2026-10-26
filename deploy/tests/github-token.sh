#!/usr/bin/env bash
# WI-2487: up.sh writes the GitHub token via store_secret, mode 0600, and
# the value never appears on stdout or stderr.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

secrets=$tmp/secrets
mkdir -p "$secrets"
token='ghs_test_token_value_not_a_real_one'
bin=$tmp/bin
mkdir -p "$bin"

cat >"$bin/secret" << 'SH'
#!/usr/bin/env bash
[[ ${1:-} == get && ${2:-} == HOMELAB_VOGT_GITHUB_TOKEN ]] || exit 2
printf '%s' "$STUB_TOKEN"
SH
chmod +x "$bin/secret"

# The live fetch_github_token pipes through docker as_root/store_secret.
# This test runs the same fetch against a local writer that matches store_secret
# (umask 077, empty-refused, 0600, atomic mv) and asserts the invocation in
# deploy/up.sh still matches.
fn=$(sed -n '/^fetch_github_token()/,/^}/p' "$root/deploy/up.sh")
printf '%s\n' "$fn" | grep -q 'HOMELAB_VOGT_GITHUB_TOKEN'
printf '%s\n' "$fn" | grep -q -- '--project 76b1ebe1-3656-4cef-952c-30d5d489c6e7'
printf '%s\n' "$fn" | grep -q -- '--env prod'
printf '%s\n' "$fn" | grep -q 'store_secret github-token'
printf '%s\n' "$fn" | grep -q 'GRAFT_SECRET_CMD:-secret'
store=$(sed -n '/^store_secret()/,/^}/p' "$root/deploy/up.sh")
printf '%s\n' "$store" | grep -q 'umask 077'
printf '%s\n' "$store" | grep -q 'chmod 0600'

log() { printf 'up.sh: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }
store_secret() {
  local dest=$secrets/$1 tmpf=$secrets/.$1.tmp
  umask 077
  cat >"$tmpf"
  if [ ! -s "$tmpf" ]; then rm -f "$tmpf"; echo 'empty value refused' >&2; exit 1; fi
  chmod 0600 "$tmpf"
  mv "$tmpf" "$dest"
}
as_root() { sh -c "${1//\/s/$secrets}"; }
eval "$(sed -n '/^fetch_github_token()/,/^}/p' "$root/deploy/up.sh")"

out=$tmp/out
err=$tmp/err
export GRAFT_SECRET_CMD=$bin/secret STUB_TOKEN=$token
PATH="$bin:$PATH" fetch_github_token >"$out" 2>"$err"
mode=$(stat -c '%a' "$secrets/github-token")
[ "$mode" = 600 ]
cmp -s "$secrets/github-token" <(printf '%s' "$token")
if grep -F "$token" "$out" "$err"; then
  echo 'token appeared in output' >&2
  exit 1
fi
grep -q 'mode 0600, not printed' "$err"
echo "github-token: mode 0600, absent from stdout and stderr"
