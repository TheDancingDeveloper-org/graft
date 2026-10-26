#!/bin/bash
# Private-mirror entrypoint (WI-2444). graft-mirror itself sends the mounted
# [github] token_file to every git command it runs against upstream, as an
# http.extraHeader in that command's environment (WI-2571), so follow and a
# separately exec'd pr-batch authenticate the same way. This script only
# warns when the token file is still empty. It never reads the token into
# argv or logs.
set -euo pipefail
# Never prompt: with no tty a prompt would hang the poll.
export GIT_TERMINAL_PROMPT=0
if [[ -z $(tr -d '\r\n' < /run/secrets/github-token 2>/dev/null) ]]; then
  # No credential yet (WI-2487): start anyway. The mirror's polls of the
  # private upstream fail and retry until /etc/graft/github-token is placed
  # and the deploy re-run.
  printf 'graft-mirror: warning: github token file is empty; following without credentials\n' >&2
fi
exec graft-mirror follow --config /etc/graft/deploy/mirror.toml
