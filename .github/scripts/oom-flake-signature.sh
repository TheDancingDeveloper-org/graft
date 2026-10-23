#!/usr/bin/env bash
# Usage: oom-flake-signature.sh LOG
# Exit 0 when a failed build log matches the known OOM flake (WI-2314):
# rustc killed under sccache, so cargo prints "could not compile" with no
# compiler diagnostic. Any other log exits 1. See docs/known-flakes.md.
# shellcheck disable=SC2016  # backticks below are literal cargo output
set -euo pipefail
clean=$(sed -E 's/(\x1b|\^\[)\[[0-9;]*[A-Za-z]//g' "$1")
grep -q 'error: could not compile' <<<"$clean" || exit 1
# A real compile failure prints error[E....] / error: diagnostics, and cargo
# ends its summary with "due to N previous errors". The only error lines the
# flake allows are the could-not-compile summaries and nextest's footer.
if grep -E '(^|[[:space:]])error(\[E[0-9]+\])?:' <<<"$clean" \
  | grep -vE 'error: could not compile `[^`]+` \([^)]*\)[[:space:]]*$' \
  | grep -vE 'error: command `.*` exited with code [0-9]+' \
  | grep -q .; then
  exit 1
fi
exit 0
