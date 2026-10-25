#!/bin/bash
# Private-mirror entrypoint (WI-2444). Reads the mounted GitHub token file
# into git's http.extraHeader. The token is never placed in argv or logs.
set -euo pipefail
token=$(tr -d '\r\n' < /run/secrets/github-token)
if [[ -z $token ]]; then
  printf 'graft-mirror: error: github token file is empty\n' >&2
  exit 1
fi
basic=$(printf 'x-access-token:%s' "$token" | base64 -w0)
unset token
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=http.extraHeader
export GIT_CONFIG_VALUE_0="Authorization: Basic ${basic}"
unset basic
exec graft-mirror follow --config /etc/graft/deploy/mirror.toml
