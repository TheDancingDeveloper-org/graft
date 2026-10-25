#!/usr/bin/env bash
# WI-2444: generated private mirrors use a block tmpfs, mode 0644 config,
# and a file-backed GitHub header. A deadline with no ref fails distinctly.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

registry=$tmp/mirrors.tsv
cat >"$registry" << 'TSV'
# service|repo|owner/repo|base|token-file|config|metrics-port|compose|private
graft-mirror|rusttorrent|TheDancingDeveloper-org/rustTorrent|main|mirror-token|mirror.toml|17890|builtin|
graft-mirror-rdpapp|rdpapp|TheDancingDeveloper-org/rdpapp|main|mirror-token-rdpapp|mirror-rdpapp.toml|17893|generated|1
TSV

out=$tmp/compose.onboarded.yml
GRAFT_MIRRORS_FILE=$registry "$root/deploy/render-onboarded-compose.sh" "$out"
grep -q 'tmpfs:' "$out"
grep -q '/tmp:rw,noexec,nosuid,size=64m' "$out"
if grep -F 'tmpfs: [' "$out"; then exit 1; fi
grep -q 'private-follow.sh' "$out"
grep -q '/run/secrets/github-token' "$out"
! grep -q 'x-access-token:' "$out" || true
# The literal token value must not be baked in. The header name is fine.
grep -q 'http.extraHeader' "$root/deploy/private-follow.sh"
echo "render: private overlay uses a block tmpfs and a file-backed header"

# Config mode: the generator's umask 022 plus chmod 0644.
cfg=$tmp/mirror-sample.toml
umask 022
cat >"$cfg" << 'CFG'
repo = "sample"
token_file = "/run/secrets/graft-mirror-token"
CFG
chmod 0644 "$cfg"
mode=$(stat -c '%a' "$cfg")
[ "$mode" = 644 ]
echo "config mode: 0644"

# no-ref deadline is not success and is not the importing log.
export GRAFT_REFS_WAIT=1
export GRAFT_REFS_POLL=1
port=$(python3 - << 'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)
python3 - "$port" << 'PY' &
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
port = int(sys.argv[1])
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"refs":[]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, fmt, *args):
        pass
ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY
stub=$!
trap 'kill "$stub" 2>/dev/null || true; rm -rf "$tmp"' EXIT
for _ in $(seq 1 50); do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/v1/repos/rdpapp/refs?limit=100" || true)
  [ "$code" != 000 ] && break
  sleep 0.05
done
base="http://127.0.0.1:$port"
export refs_wait=$GRAFT_REFS_WAIT
log() { printf 'up.sh: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }
refs() { curl -fsS "$base/v1/repos/$1/refs?limit=100"; }
eval "$(sed -n '/^wait_for_refs()/,/^}/p' "$root/deploy/up.sh")"
set +e
err=$(wait_for_refs graft-mirror-rdpapp rdpapp token 2>&1)
rc=$?
set -e
[ "$rc" -ne 0 ]
printf '%s\n' "$err" | grep -q 'no ref after import'
printf '%s\n' "$err" | grep -q 'error: rdpapp has no ref after import'
echo "no-ref: failed distinctly (exit $rc)"
