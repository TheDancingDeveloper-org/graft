#!/usr/bin/env bash
# Scripted two-agent demo, and the docs-smoke check for it.
#
# Two working copies land disjoint edits (the second line-merges), then
# overlapping edits (the second exits 3). The conflict is shown, resolved
# and landed. Delivery to git is `graft mirror export --to` a local bare
# repo: `pr-batch` needs a mirror config and a forge API, so it cannot
# target a bare repo by itself.
#
# Loopback only. No token is printed. No build, test or check runs inside
# the flow.
set -euo pipefail

root=$(cd -- "$(dirname -- "$0")/../.." && pwd)
graft=${GRAFT_BIN:-graft}
if [[ $graft != /* ]]; then
  graft=$(command -v "$graft")
fi
if [[ ! -x $graft ]]; then
  echo "demo: graft binary not executable" >&2
  exit 1
fi

work=$(mktemp -d)
server_pid=
log=$work/transcript

cleanup() {
  local status=$?
  if [[ -n $server_pid ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

say() { printf '## %s\n' "$*" | tee -a "$log"; }

run() {
  echo "+ $(basename "$1") ${*:2}" | tee -a "$log"
  "$@" 2>&1 | tee -a "$log"
}

# serve --dev writes an admin token into the user config. Keep that config,
# and the token file, inside the temp dir so nothing is left behind.
config=$work/config.toml
export GRAFT_CONFIG=$config
export XDG_CONFIG_HOME=$work/xdg
mkdir -p "$XDG_CONFIG_HOME"
unset GRAFT_ENDPOINT GRAFT_TOKEN GRAFT_AUTHOR_NAME GRAFT_AUTHOR_EMAIL || true

say "start graft serve --dev on loopback"
"$graft" serve --dev \
  --data "$work/data" \
  --state "$work/state" \
  --repo demo \
  --listen 127.0.0.1:0 \
  >"$work/serve.log" 2>&1 &
server_pid=$!

url=
for _ in $(seq 1 100); do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "demo: graft serve --dev exited" >&2
    cat "$work/serve.log" >&2 || true
    exit 1
  fi
  if grep -q '^graft clone graft://' "$work/serve.log"; then
    url=$(grep -m1 '^graft clone graft://' "$work/serve.log" | sed 's/^graft clone //')
    break
  fi
  sleep 0.1
done
if [[ -z $url ]]; then
  echo "demo: serve --dev did not print a clone command" >&2
  exit 1
fi
case $url in
  graft://127.0.0.1:*|graft://localhost:*) ;;
  *) echo "demo: clone url is not loopback" >&2; exit 1 ;;
esac

say "clone two working copies"
run "$graft" clone "$url" "$work/a"
run "$graft" clone "$url" "$work/b"

say "shared base"
printf 'one\ntwo\nthree\n' > "$work/a/file.txt"
( cd "$work/a" && run "$graft" commit -m "shared base" && run "$graft" land )
( cd "$work/b" && run "$graft" sync )

say "disjoint edits: the second land line-merges"
printf 'alpha\ntwo\nthree\n' > "$work/a/file.txt"
printf 'one\ntwo\nomega\n' > "$work/b/file.txt"
( cd "$work/a" && run "$graft" commit -m "region one" && run "$graft" land )
land_out=$(
  cd "$work/b"
  "$graft" commit -m "region two"
  "$graft" land
) || land_status=$?
land_status=${land_status:-0}
printf '%s\n' "$land_out" | tee -a "$log"
if [[ $land_status -ne 0 ]]; then
  echo "demo: disjoint land exited $land_status, want 0" >&2
  exit 1
fi
got=$(cat "$work/b/file.txt")
want=$(printf 'alpha\ntwo\nomega\n')
if [[ $got != "$want" ]]; then
  echo "demo: line merge did not keep both edits" >&2
  exit 1
fi
( cd "$work/a" && run "$graft" sync )

say "same region: the second land conflicts (exit 3)"
printf 'LEFT\ntwo\nomega\n' > "$work/a/file.txt"
printf 'RIGHT\ntwo\nomega\n' > "$work/b/file.txt"
( cd "$work/a" && run "$graft" commit -m "left" && run "$graft" land )
( cd "$work/b" && run "$graft" commit -m "right" )
set +e
conflict_out=$(
  cd "$work/b"
  "$graft" land
  echo "exit:$?"
)
set -e
printf '%s\n' "$conflict_out" | tee -a "$log"
conflict_status=${conflict_out##*exit:}
if [[ $conflict_status != 3 ]]; then
  echo "demo: conflict land exited $conflict_status, want 3" >&2
  exit 1
fi
# The pre-land check sends nothing. `sync` writes the markers and exits 3.
set +e
sync_out=$( cd "$work/b" && "$graft" sync )
sync_status=$?
set -e
printf '%s\n' "$sync_out" | tee -a "$log"
if [[ $sync_status -ne 3 ]]; then
  echo "demo: sync after a conflict exited $sync_status, want 3" >&2
  exit 1
fi
say "conflict markers"
if ! grep -q '<<<<<<<' "$work/b/file.txt"; then
  echo "demo: conflict markers missing" >&2
  cat "$work/b/file.txt" >&2 || true
  exit 1
fi
tee -a "$log" < "$work/b/file.txt"

say "resolve and land"
printf 'BOTH\ntwo\nomega\n' > "$work/b/file.txt"
( cd "$work/b" && run "$graft" commit -m "resolved" && run "$graft" land )
resolved=$(cat "$work/b/file.txt")
if [[ $resolved != "$(printf 'BOTH\ntwo\nomega\n')" ]]; then
  echo "demo: resolved content did not land" >&2
  exit 1
fi

say "deliver to a local bare git repo"
# pr-batch needs a mirror config and a forge API, so a bare repo is
# reached with `mirror export --to` instead.
git init --bare "$work/bare.git" >>"$log" 2>&1
db=$work/data/meta.db
if [[ ! -f $db ]]; then
  echo "demo: metadata database missing" >&2
  exit 1
fi
token_file=$(awk -F'"' '/token_file/ { print $2; exit }' "$config")
http_url=${url/graft:\/\//http://}
run "$graft" mirror export --to "$work/bare.git" --repo demo --db "$db" \
  --server "$http_url" --token-file "$token_file"
say "git log"
git -C "$work/bare.git" log --oneline main | tee -a "$log"
count=$(git -C "$work/bare.git" rev-list --count main)
if [[ $count -lt 1 ]]; then
  echo "demo: bare repo has no commits" >&2
  exit 1
fi

say "scan output"
if grep -E -i -n 'ghp_|github_pat_|gho_|sk-|AKIA|sprooty|indexarr|100\.[0-9]+\.[0-9]+\.[0-9]+' "$log"; then
  echo "demo: transcript matched a token or an estate host" >&2
  exit 1
fi
say "ok"
