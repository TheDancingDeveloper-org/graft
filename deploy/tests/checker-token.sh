#!/usr/bin/env bash
# up.sh's install_checker_github_token: the post-land checker's own GitHub
# token is piped from Infisical (the `secret` tool) into
# github-checker-token through store_secret, 0600 and owned by 10001:10001;
# without `secret`, a copy placed on the host is used; with neither, it
# reports not ready so up.sh leaves the checker off. The token never appears
# in output or in the `secret` tool's arguments, and the mirrors'
# github-token is never touched.
#
# The real up.sh functions run here. Only as_root is replaced, by a fake
# root that maps /s to a temp directory and records chown.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

token='github_pat_test_value_not_a_real_one'
fail() { echo "FAIL: $*" >&2; exit 1; }

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

# A fake `secret`: prints FAKE_SECRET_VALUE for `get`, or fails when it is
# unset. Its arguments are logged, to check the value is never among them.
fakes=$tmp/fakes
mkdir -p "$fakes"
cat >"$fakes/secret" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$FAKE_SECRET_ARGS"
[ "$1" = get ] || exit 2
[ -n "${FAKE_SECRET_VALUE:-}" ] || { echo "secret: $2 not found" >&2; exit 1; }
printf '%s' "$FAKE_SECRET_VALUE"
SH
chmod +x "$fakes/secret"
export FAKE_SECRET_ARGS=$tmp/secret-args

as_root() {
  local snippet=${1//\/s\//$secrets/}
  PATH="$bin:$PATH" sh -c "$snippet"
}
log() { printf 'up.sh: %s\n' "$*" >&2; }
for f in store_secret install_checker_github_token; do
  body=$(sed -n "/^$f()/,/^}/p" "$root/deploy/up.sh")
  [ -n "$body" ] || fail "up.sh has no $f"
  eval "$body"
done

new_host() {
  secrets=$tmp/host-$1
  mkdir -p "$secrets"
  rm -f "$FAKE_OWNERS"/* "$FAKE_SECRET_ARGS"
  printf 'mirror-token-value\n' >"$secrets/github-token"
  chmod 0600 "$secrets/github-token"
  checker_github_ready=9
}
run() {
  # $1: PATH for the step (with or without the fake `secret`).
  PATH=$1 install_checker_github_token >"$tmp/out" 2>"$tmp/err"
}
no_token_in() {
  if grep -F -- "$token" "$@" 2>/dev/null; then fail "token appeared in output or argv"; fi
}
mirror_untouched() {
  [ "$(cat "$secrets/github-token")" = mirror-token-value ] || fail "the mirrors' github-token changed"
  [ ! -e "$FAKE_OWNERS/github-token" ] || fail "the mirrors' github-token was chowned"
}
with_secret="$fakes:$bin:/usr/bin:/bin"
without_secret="$bin:/usr/bin:/bin"

# --- in Infisical -----------------------------------------------------------
new_host infisical
FAKE_SECRET_VALUE=$token run "$with_secret"
[ "$checker_github_ready" = 1 ] || fail "infisical: ready=$checker_github_ready"
[ "$(cat "$secrets/github-checker-token")" = "$token" ] || fail "infisical: content"
[ "$(stat -c %a "$secrets/github-checker-token")" = 600 ] || fail "infisical: mode"
[ "$(cat "$FAKE_OWNERS/github-checker-token")" = 10001:10001 ] || fail "infisical: owner"
grep -q '^get GRAFT_DEV_CHECKER_GITHUB_TOKEN --project apps$' "$FAKE_SECRET_ARGS" || fail "infisical: secret args"
grep -q 'placed from Infisical' "$tmp/err" || fail "infisical: no log line"
no_token_in "$tmp/out" "$tmp/err" "$FAKE_SECRET_ARGS"
mirror_untouched
echo "ok: from Infisical via stdin: 0600, 10001:10001, not printed, mirrors' token untouched"

# --- Infisical has none, the host has a copy --------------------------------
new_host fallback
printf '%s\n' "$token" >"$secrets/github-checker-token"
chmod 0644 "$secrets/github-checker-token"
FAKE_SECRET_VALUE='' run "$with_secret"
[ "$checker_github_ready" = 1 ] || fail "fallback: ready=$checker_github_ready"
[ "$(cat "$secrets/github-checker-token")" = "$token" ] || fail "fallback: an empty read replaced the host copy"
[ "$(stat -c %a "$secrets/github-checker-token")" = 600 ] || fail "fallback: mode not fixed"
[ "$(cat "$FAKE_OWNERS/github-checker-token")" = 10001:10001 ] || fail "fallback: owner"
grep -q 'using the copy on this host' "$tmp/err" || fail "fallback: no log line"
no_token_in "$tmp/out" "$tmp/err"
mirror_untouched
echo "ok: Infisical empty: the host copy is kept, set to 0600 and 10001:10001"

# --- no secret tool, host copy (the graft-dev runner) -----------------------
new_host runner
printf '%s\n' "$token" >"$secrets/github-checker-token"
run "$without_secret"
[ "$checker_github_ready" = 1 ] || fail "runner: ready=$checker_github_ready"
[ ! -e "$FAKE_SECRET_ARGS" ] || fail "runner: secret was called"
no_token_in "$tmp/out" "$tmp/err"
echo "ok: no secret tool: the host copy is used"

# --- absent everywhere ------------------------------------------------------
for kind in missing empty blank; do
  new_host "absent-$kind"
  case $kind in
    empty) : >"$secrets/github-checker-token" ;;
    blank) printf '\n \n' >"$secrets/github-checker-token" ;;
  esac
  FAKE_SECRET_VALUE='' run "$with_secret"
  [ "$checker_github_ready" = 0 ] || fail "absent-$kind: ready=$checker_github_ready"
  mirror_untouched
  run "$without_secret"
  [ "$checker_github_ready" = 0 ] || fail "absent-$kind (no secret tool): ready=$checker_github_ready"
done
echo "ok: absent in Infisical and on the host: not ready, so the checker stays off"
