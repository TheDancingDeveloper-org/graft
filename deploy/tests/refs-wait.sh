#!/usr/bin/env bash
# Exercise wait_for_refs against a stub endpoint (WI-2419).
#   empty-then-refs: no refs for a few polls, then one ref → success
#   endpoint-error: HTTP 500 → non-zero, and the error is logged
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d)
port=$(python3 - << 'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)
mode=${1:-empty-then-refs}
python3 - "$port" "$mode" << 'PY' &
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
port, mode = int(sys.argv[1]), sys.argv[2]
state = {"n": 0}

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        state["n"] += 1
        if mode == "endpoint-error":
            self.send_response(500)
            self.end_headers()
            self.wfile.write(b"boom")
            return
        body = b'{"refs":[]}' if state["n"] < 3 else b'{"refs":[{"name":"main"}]}'
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

ready=0
for _ in $(seq 1 50); do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/v1/repos/vogt/refs?limit=100" || true)
  if [ "$code" != "000" ]; then
    ready=1
    break
  fi
  sleep 0.05
done
[ "$ready" -eq 1 ] || { echo "stub did not start" >&2; exit 1; }

export GRAFT_REFS_WAIT=30
export GRAFT_REFS_POLL=1
base="http://127.0.0.1:$port"
refs_wait=$GRAFT_REFS_WAIT  # consumed by wait_for_refs

log() { printf 'up.sh: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }
refs() { curl -fsS "$base/v1/repos/$1/refs?limit=100"; }

# shellcheck source=/dev/null
eval "$(sed -n '/^wait_for_refs()/,/^}/p' "$root/deploy/up.sh")"

set +e
wait_for_refs graft-mirror-vogt vogt token
rc=$?
set -e
if [ "$mode" = endpoint-error ]; then
  [ "$rc" -ne 0 ]
  echo "endpoint-error: failed as expected (exit $rc)"
else
  [ "$rc" -eq 0 ]
  echo "empty-then-refs: passed"
fi
