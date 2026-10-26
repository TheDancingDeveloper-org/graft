#!/bin/bash
# Private-mirror entrypoint (WI-2444). Reads the mounted GitHub token file
# into git's http.extraHeader. The token is never placed in argv or logs.
set -euo pipefail
token=$(tr -d '\r\n' < /run/secrets/github-token)
# Never prompt: with no tty a prompt would hang the poll.
export GIT_TERMINAL_PROMPT=0
if [[ -z $token ]]; then
  # No credential yet (WI-2487): start anyway. The mirror's polls of the
  # private upstream fail and retry until /etc/graft/github-token is placed
  # and the deploy re-run.
  printf 'graft-mirror: warning: github token file is empty; following without credentials\n' >&2
  exec graft-mirror follow --config /etc/graft/deploy/mirror.toml
fi
basic=$(printf 'x-access-token:%s' "$token" | base64 -w0)
unset token
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=http.extraHeader
export GIT_CONFIG_VALUE_0="Authorization: Basic ${basic}"
unset basic
exec graft-mirror follow --config /etc/graft/deploy/mirror.toml
