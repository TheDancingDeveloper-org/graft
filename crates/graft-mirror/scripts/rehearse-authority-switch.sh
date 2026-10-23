#!/usr/bin/env bash
# Rehearse the authority switch (docs/design.md §11.3, plan item G3.9) end
# to end with the real binaries: a full A -> B -> A run on a scratch copy of
# a git repository, with a local bare repository standing in for GitHub.
#
# usage: rehearse-authority-switch.sh <source-git-repo> [work-dir]
#
#   source-git-repo  a clone of the repository to rehearse on (rustTorrent);
#                    only its BRANCH (default main) is copied, never pushed to
#   work-dir         scratch directory (default: a new mktemp -d); kept
#
# Binaries come from GRAFT_BIN (default: target/debug of this checkout):
# graft-server, graft-mirror and graft. Nothing leaves the host: the server
# listens on 127.0.0.1, the "GitHub" is a path, and the token stays in a
# 0600 file in the work directory.
#
# Exits 0 when the run lost nothing and ended sha-identical, 1 otherwise.
set -euo pipefail

src="${1:?usage: rehearse-authority-switch.sh <source-git-repo> [work-dir]}"
work="${2:-$(mktemp -d -t graft-authority-rehearsal.XXXXXX)}"
branch="${BRANCH:-main}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
bin="${GRAFT_BIN:-$root/target/debug}"
port="${PORT:-17878}"
repo=rehearsal
# The graft line. Not `main`: D6 makes graft's `main` attested, and the
# rehearsal has no CI to attest; every other line is `direct`.
line="${LINE:-trunk}"
people=0
native=0

mkdir -p "$work"
work="$(cd "$work" && pwd)"
log() { printf '\n== %s\n' "$*"; }
fail() { printf 'REHEARSAL FAILED: %s\n' "$*" >&2; exit 1; }

pids=()
cleanup() {
  for p in "${pids[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT

# git with a fixed identity and no user configuration.
g() {
  GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
  GIT_AUTHOR_NAME="Rehearsal Person" GIT_AUTHOR_EMAIL=person@example.com \
  GIT_COMMITTER_NAME="Rehearsal Person" GIT_COMMITTER_EMAIL=person@example.com \
    git -c commit.gpgsign=false "$@"
}

mirror() { "$bin/graft-mirror" "$@"; }
authority() { mirror authority "$1" --config "$work/mirror.toml" --line "$line" "${@:2}"; }
status_level() { authority status | grep -q '^  level:     yes$'; }

# Waits (bounded) until graft and the "GitHub" name the same commit.
wait_level() {
  for _ in $(seq 1 120); do
    status_level && return 0
    sleep 1
  done
  authority status || true
  fail "graft and upstream did not level within 120 polls"
}

github_main() { g -C "$work/github.git" rev-parse "refs/heads/$branch"; }

# A person pushes one commit to the "GitHub". Returns git push's status.
person_push() {
  people=$((people + 1))
  g -C "$work/person" fetch -q origin
  g -C "$work/person" reset -q --hard "origin/$branch"
  printf 'pushed by a person, %s\n' "$people" > "$work/person/REHEARSAL-person-$people.txt"
  g -C "$work/person" add "REHEARSAL-person-$people.txt"
  g -C "$work/person" commit -q -m "rehearsal: person $people"
  g -C "$work/person" push -q origin "HEAD:refs/heads/$branch"
}

# Lands one native change through graft.
graft_land() {
  native=$((native + 1))
  (
    cd "$work/wc"
    "$bin/graft" sync >/dev/null
    printf 'landed in graft, %s\n' "$native" > "REHEARSAL-native-$native.txt"
    "$bin/graft" commit -m "rehearsal: native $native" >/dev/null
    "$bin/graft" land >/dev/null
  )
}

start_follow() {
  mirror follow --config "$work/mirror.toml" >>"$work/follow.log" 2>&1 &
  follow_pid=$!
  pids+=("$follow_pid")
}

start_export() {
  mirror export --config "$work/mirror.toml" >>"$work/export.log" 2>&1 &
  export_pid=$!
  pids+=("$export_pid")
}

stop() { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }

log "scratch copy of $src ($branch) in $work"
g clone -q --bare --no-tags --single-branch --branch "$branch" "$src" "$work/github.git"
g -C "$work/github.git" config receive.denyNonFastForwards true
g -C "$work/github.git" config receive.autogc false
g -C "$work/github.git" config gc.auto 0
original="$(github_main)"
original_count="$(g -C "$work/github.git" rev-list --count "$original")"
echo "upstream $branch at $original ($original_count commits)"
g clone -q "$work/github.git" "$work/person"

log "graft-server on 127.0.0.1:$port"
mkdir -p "$work/server"
cat >"$work/server.toml" <<EOF
listen = "127.0.0.1:$port"
objects_dir = "$work/server/objects"
meta_db = "$work/server/meta.db"
EOF
"$bin/graft-server" --config "$work/server.toml" serve >>"$work/server.log" 2>&1 &
pids+=("$!")
for _ in $(seq 1 60); do
  curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null || fail "the server did not come up"
"$bin/graft-server" --config "$work/server.toml" admin repo create "$repo" >/dev/null
umask 077
"$bin/graft-server" --config "$work/server.toml" admin token mint --subject rehearsal \
  --kind service --scope "repo:$repo:admin" --scope "repo:$repo:read" \
  --scope "repo:$repo:write" --scope "repo:$repo:land:$line" --ttl 7200 >"$work/token"
umask 022
export GRAFT_ENDPOINT="http://127.0.0.1:$port"

cat >"$work/mirror.toml" <<EOF
server = "http://127.0.0.1:$port"
repo = "$repo"
upstream = "$work/github.git"
mirror_dir = "$work/mirror.git"
db = "$work/mirror.sqlite"
token_file = "$work/token"
poll_interval_secs = 1

[[lines]]
branch = "$branch"
line = "$line"
EOF

log "mode A (git-primary): follow imports the history"
start_follow
wait_level
for _ in 1 2 3; do person_push; done
wait_level
authority status

log "A -> B: freeze"
authority freeze
if person_push 2>/dev/null; then fail "a person pushed to a frozen branch"; fi
people=$((people - 1))
echo "a person's push was refused, as it should be"
log "A -> B: verify"
authority verify --last 100
log "A -> B: flip"
authority flip
stop "$follow_pid"
start_export
log "A -> B: unfreeze"
authority unfreeze
authority status

log "mode B (graft-primary): graft lands, export pushes"
GRAFT_TOKEN="$(cat "$work/token")" "$bin/graft" clone "$repo" "$work/wc" --line "$line" --path REHEARSAL-dir --root-files >/dev/null
export GRAFT_TOKEN="$(cat "$work/token")"
for _ in 1 2 3; do graft_land; done
wait_level
if person_push 2>/dev/null; then fail "a person pushed to a graft-primary branch"; fi
people=$((people - 1))
echo "a person's push was refused, as it should be"
authority status

log "B -> A: freeze (one land still in flight)"
authority freeze
graft_land
wait_level
log "B -> A: verify"
authority verify --last 100
log "B -> A: flip"
authority flip
stop "$export_pid"
start_follow
log "B -> A: unfreeze"
authority unfreeze

log "mode A again: people push on top of graft's history"
for _ in 1 2; do person_push; done
wait_level
stop "$follow_pid"
authority status
authority verify --last 100

log "checks"
final="$(github_main)"
g -C "$work/github.git" merge-base --is-ancestor "$original" "$final" \
  || fail "the original history is no longer under $branch"
added="$(g -C "$work/github.git" rev-list --count --first-parent "$original..$final")"
want=$((people + native))
[ "$added" -eq "$want" ] || fail "upstream gained $added commits, expected $want ($people by people, $native native)"
echo "upstream kept all $original_count original commits and gained $added ($people by people, $native native)"
g -C "$work/github.git" fsck --no-progress --no-dangling >/dev/null || fail "git fsck"
mirror export --to "$work/export.git" --repo "$repo" --db "$work/mirror.sqlite" \
  --server "$GRAFT_ENDPOINT" --token-file "$work/token" >/dev/null
exported="$(g -C "$work/export.git" rev-parse "refs/heads/$line")"
[ "$exported" = "$final" ] || fail "a full export of graft gives $exported, upstream holds $final"
echo "a full export of graft's $line is sha-identical to upstream: $final"
log "REHEARSAL PASSED"
