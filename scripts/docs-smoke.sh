#!/usr/bin/env bash
# Run the two README quick-start blocks (between docs-smoke markers) against
# a built graft binary. Outcomes only: every step exits 0, and a second land
# from another working copy line-merges (merge3) into the file.
set -euo pipefail

root=$(cd -- "$(dirname -- "$0")/.." && pwd)
readme=${DOCS_SMOKE_README:-$root/README.md}
graft=${GRAFT_BIN:-graft}

if [[ ! -x $graft && $graft != */* ]]; then
  if command -v "$graft" >/dev/null 2>&1; then
    :
  else
    echo "docs-smoke: graft binary not executable: $graft" >&2
    exit 1
  fi
fi

extract() {
  local name=$1
  awk -v name="$name" '
    $0 == "<!-- docs-smoke: " name " -->" { inside = 1; next }
    $0 == "<!-- /docs-smoke: " name " -->" { inside = 0 }
    inside && /^```/ { fence = !fence; next }
    inside && fence { print }
  ' "$readme"
}

run_block() {
  local name=$1 dir=$2
  local script
  script=$(extract "$name")
  if [[ -z $script ]]; then
    echo "docs-smoke: no quick-start block '$name' in $readme" >&2
    exit 1
  fi
  (
    cd "$dir"
    # shellcheck disable=SC2094
    bash -euo pipefail -c "$script"
  )
}

work=$(mktemp -d)
cleanup() {
  local status=$?
  if [[ -n ${server_pid:-} ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

export PATH
bin_dir=$(dirname -- "$graft")
if [[ $graft == */* ]]; then
  PATH=$bin_dir:$PATH
fi

echo "docs-smoke: local quick start"
run_block local "$work"

# The README prose after the local block: a second copy edits a different
# region, commits and lands. Line merge keeps both edits.
copy=$work/demo-2
# The second copy was cloned before the first land. Sync, then edit a
# different region (a new first line) and land. Line merge keeps both.
(
  cd "$copy"
  graft sync
  printf 'also\none\ntwo\n' > file.txt
  graft commit -m "another region"
  graft land
)
got=$(cat "$copy/file.txt")
want=$(printf 'also\none\ntwo\n')
if [[ $got != "$want" ]]; then
  echo "docs-smoke: second land did not line-merge" >&2
  printf 'got:\n%s\n' "$got" >&2
  exit 1
fi
echo "docs-smoke: second land line-merged"

echo "docs-smoke: server quick start"
server_home=$work/xdg
mkdir -p "$server_home"
serve_log=$work/serve.log
# shellcheck disable=SC2094
GRAFT_CONFIG=$server_home/config.toml \
  XDG_CONFIG_HOME=$server_home \
  graft serve --dev >"$serve_log" 2>&1 &
server_pid=$!

url=
for _ in $(seq 1 100); do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "docs-smoke: graft serve --dev exited" >&2
    cat "$serve_log" >&2 || true
    exit 1
  fi
  if grep -q '^graft clone graft://' "$serve_log"; then
    url=$(grep -m1 '^graft clone graft://' "$serve_log" | sed 's/^graft clone //')
    break
  fi
  sleep 0.1
done
if [[ -z $url ]]; then
  echo "docs-smoke: serve --dev did not print a clone command" >&2
  cat "$serve_log" >&2 || true
  exit 1
fi

client=$work/client
mkdir -p "$client"
script=$(extract server)
script=${script//graft:\/\/127.0.0.1:PORT\/dev/$url}
(
  cd "$client"
  GRAFT_CONFIG=$server_home/config.toml \
    XDG_CONFIG_HOME=$server_home \
    bash -euo pipefail -c "$script"
)
hello=$(cat "$client/dev/readme.txt")
if [[ $hello != "$(printf 'hello\n')" ]]; then
  echo "docs-smoke: server quick start did not write readme.txt" >&2
  exit 1
fi
echo "docs-smoke: ok"
