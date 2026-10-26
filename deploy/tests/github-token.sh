#!/usr/bin/env bash
# WI-2487: up.sh copies /etc/graft/github-token (mode 0600) and does not
# fail the deploy when that file is missing. The token never appears on
# stdout or stderr.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

token='ghs_test_token_value_not_a_real_one'

fn=$(sed -n '/^install_github_token()/,/^}/p' "$root/deploy/up.sh")
printf '%s\n' "$fn" | grep -q '/etc/graft/github-token'
printf '%s\n' "$fn" | grep -q 'store_secret github-token'
if printf '%s\n' "$fn" | grep -q 'HOMELAB_VOGT_GITHUB_TOKEN'; then
  echo 'install_github_token still fetches from Infisical' >&2
  exit 1
fi
strip=$(sed -n '/^strip_github_table()/,/^}/p' "$root/deploy/up.sh")
printf '%s\n' "$strip" | grep -q 'github'

log() { printf 'up.sh: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

# Local stand-in for as_root: the script's /s is the secrets directory.
as_root() { sh -c "${1//\/s/$secrets}"; }
store_secret() {
  local dest=$secrets/$1 tmpf=$secrets/.$1.tmp
  umask 077
  cat >"$tmpf"
  if [ ! -s "$tmpf" ]; then rm -f "$tmpf"; echo 'empty value refused' >&2; exit 1; fi
  chmod 0600 "$tmpf"
  mv "$tmpf" "$dest"
}
eval "$(sed -n '/^install_github_token()/,/^}/p' "$root/deploy/up.sh")"
eval "$(sed -n '/^strip_github_table()/,/^}/p' "$root/deploy/up.sh")"

present=$tmp/present
missing=$tmp/missing
mkdir -p "$present" "$missing"

# Rewrite the hardcoded source path for the present case, then the missing one.
run_install() {
  local src=$1
  secrets=$2
  mkdir -p "$secrets"
  install_github_token "$src"
}

out=$tmp/out
err=$tmp/err

# Present: copied, mode 0600, value absent from both streams.
mkdir -p "$(dirname "$present/src")"
printf '%s' "$token" >"$present/src"
secrets=$present/secrets
mkdir -p "$secrets"
run_install "$present/src" "$secrets" >"$out" 2>"$err"
[ "${github_ready:-0}" = 1 ]
mode=$(stat -c '%a' "$secrets/github-token")
[ "$mode" = 600 ]
cmp -s "$secrets/github-token" <(printf '%s' "$token")
if grep -F "$token" "$out" "$err"; then
  echo 'token appeared in output' >&2
  exit 1
fi
grep -q 'mode 0600, not printed' "$err"
echo "github-token present: mode 0600, absent from stdout and stderr"

# Missing: deploy step succeeds, one warning, no token text, empty mount file.
: >"$out"
: >"$err"
github_ready=9
secrets=$missing/secrets
mkdir -p "$secrets"
run_install "$missing/does-not-exist" "$secrets" >"$out" 2>"$err"
[ "${github_ready:-0}" = 0 ]
[ -f "$secrets/github-token" ]
[ ! -s "$secrets/github-token" ]
mode=$(stat -c '%a' "$secrets/github-token")
[ "$mode" = 600 ]
grep -q 'missing or empty' "$err"
if grep -F "$token" "$out" "$err"; then
  echo 'token appeared in the missing-file output' >&2
  exit 1
fi

# The mounted config drops [github] and keeps [[lines]].
sample=$tmp/sample.toml
cat >"$sample" << 'TOML'
server = "http://graft-server:7878"
token_file = "/run/secrets/graft-mirror-token"

[github]
owner = "org"
repo = "name"
token_file = "/run/secrets/github-token"
[[lines]]
branch = "main"
line = "main"
TOML
stripped=$(strip_github_table <"$sample")
printf '%s\n' "$stripped" | grep -q '\[\[lines\]\]'
if printf '%s\n' "$stripped" | grep -q '\[github\]'; then
  echo 'strip left [github]' >&2
  exit 1
fi
echo "github-token missing: warning logged, deploy step succeeded, [github] omitted"
