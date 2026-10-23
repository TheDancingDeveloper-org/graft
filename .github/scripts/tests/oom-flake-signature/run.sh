#!/usr/bin/env bash
# Checks oom-flake-signature.sh against sample logs: every log in retry/
# must match, every log in fail/ must not.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
sig="$here/../../oom-flake-signature.sh"
bad=0
for f in "$here"/retry/*.log; do
  if "$sig" "$f"; then echo "ok   retry $(basename "$f")"; else echo "FAIL retry $(basename "$f")"; bad=1; fi
done
for f in "$here"/fail/*.log; do
  if "$sig" "$f"; then echo "FAIL fail  $(basename "$f")"; bad=1; else echo "ok   fail  $(basename "$f")"; fi
done
exit "$bad"
