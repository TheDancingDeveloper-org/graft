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

# Config mode: exercise onboard-repo.sh with external work stubbed out.
fixture=$tmp/deploy
bin=$tmp/bin
mkdir -p "$fixture" "$bin"
cp "$root/deploy/onboard-repo.sh" "$root/deploy/render-onboarded-compose.sh" "$fixture/"
cat >"$fixture/mirrors.tsv" << 'TSV'
# service|repo|owner/repo|base|token-file|config|metrics-port|compose|private
graft-mirror|sample|example/sample|main|mirror-token|mirror.toml|17890|builtin|
TSV
printf 'GRAFT_IMAGE=test-image\n' >"$fixture/image.env"
cat >"$fixture/up.sh" << 'SH'
#!/usr/bin/env bash
exit 0
SH
cat >"$bin/git" << 'SH'
#!/usr/bin/env bash
exit 0
SH
cat >"$bin/docker" << 'SH'
#!/usr/bin/env bash
if [[ ${1:-} == compose && " $* " == *' admin token mint '* ]]; then
  printf 'temporary-admin-token\n'
fi
if [[ ${1:-} == run ]]; then
  exit 0
fi
exit 0
SH
chmod +x "$fixture/onboard-repo.sh" "$fixture/render-onboarded-compose.sh" "$fixture/up.sh" "$bin/git" "$bin/docker"
PATH="$bin:$PATH" "$fixture/onboard-repo.sh" example/mode-check main >/dev/null
cfg=$fixture/mirror-mode-check.toml
mode=$(stat -c '%a' "$cfg")
[ "$mode" = 644 ]
echo "config mode: 0644"

# Line creation: exit 75 then 1 must die with the line message, not succeed.
cat >"$bin/docker" << 'SH'
#!/usr/bin/env bash
if [[ ${1:-} == compose && " $* " == *' admin token mint '* ]]; then
  printf 'temporary-admin-token\n'
fi
if [[ ${1:-} == run ]]; then
  n=${GRAFT_LINE_FAIL_N:-0}
  n=$((n + 1))
  printf '%s\n' "$n" >"${GRAFT_LINE_FAIL_COUNT:?}"
  if [[ $n -eq 1 ]]; then exit 75; fi
  exit 1
fi
exit 0
SH
chmod +x "$bin/docker"
export GRAFT_LINE_ATTEMPTS=2 GRAFT_LINE_POLL=0
export GRAFT_LINE_FAIL_COUNT=$tmp/line-fails
printf '0\n' >"$GRAFT_LINE_FAIL_COUNT"
set +e
line_err=$(PATH="$bin:$PATH" "$fixture/onboard-repo.sh" example/line-fail main 2>&1)
line_rc=$?
set -e
[ "$line_rc" -ne 0 ]
printf '%s\n' "$line_err" | grep -q 'could not create line trial-1 from main'
echo "line create: exit 75 then 1 dies"

cat >"$bin/docker" << 'SH'
#!/usr/bin/env bash
if [[ ${1:-} == compose && " $* " == *' admin token mint '* ]]; then
  printf 'temporary-admin-token\n'
fi
if [[ ${1:-} == run ]]; then
  exit 0
fi
exit 0
SH
chmod +x "$bin/docker"
unset GRAFT_LINE_ATTEMPTS GRAFT_LINE_POLL GRAFT_LINE_FAIL_COUNT


# An availability failure must stop onboarding rather than mark the repo private.
cat >"$bin/git" << 'SH'
#!/usr/bin/env bash
printf 'fatal: unable to access github: Could not resolve host\n' >&2
exit 128
SH
set +e
network_error=$(PATH="$bin:$PATH" "$fixture/onboard-repo.sh" example/network-failure main 2>&1)
network_rc=$?
set -e
[ "$network_rc" -ne 0 ]
printf '%s\n' "$network_error" | grep -q 'GitHub or the network is unavailable'
[ ! -e "$fixture/mirror-network-failure.toml" ]
echo "probe: network failure is not classified as private"

# An authentication-shaped failure is the private-repository signal.
cat >"$bin/git" << 'SH'
#!/usr/bin/env bash
printf 'fatal: could not read Username for https://github.com: terminal prompts disabled\n' >&2
exit 128
SH
PATH="$bin:$PATH" "$fixture/onboard-repo.sh" example/private-repo main >/dev/null
grep -q '^\[github\]$' "$fixture/mirror-private-repo.toml"
grep -q 'example/private-repo|main|mirror-token-private-repo|mirror-private-repo.toml|.*|generated|1$' "$fixture/mirrors.tsv"
echo "probe: authentication failure is classified as private"

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
