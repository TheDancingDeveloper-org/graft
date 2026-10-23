#!/usr/bin/env bash
# Usage: oom-flake-signature.sh LOG
# Exit 0 when a failed nextest build log matches the known OOM flake
# (WI-2314); any other log exits 1. See docs/known-flakes.md. Forms:
#  (a) rustc killed under sccache: "could not compile" with no diagnostic;
#  (b) cargo itself killed: the only error is the final
#      "cargo ... --no-run ... exited" line (with code 101, or no code
#      at all when cargo was killed by a signal);
#  (c) the linker killed: every "error: linking with" has a matching
#      "ld terminated with signal 9" (or "signal: 9, SIGKILL") note.
# Never the flake: an error[E....] or other error: diagnostic, or a test
# failure. Tests: .github/scripts/tests/oom-flake-signature/run.sh
# shellcheck disable=SC2016  # backticks below are literal cargo output
set -euo pipefail
clean=$(sed -E 's/(\x1b|\^\[)\[[0-9;]*[A-Za-z]//g' "$1")

could_not_compile='error: could not compile `[^`]+` \([^)]*\)[[:space:]]*$'
due_to_previous='error: could not compile `[^`]+` \([^)]*\) due to [0-9]+ previous errors?(; [0-9]+ warnings? emitted)?[[:space:]]*$'
cargo_no_run='error: command `.*cargo .*--no-run.*` exited( with code 101)?[[:space:]]*$'
link_error='error: linking with `[^`]+` failed'
link_killed='ld terminated with signal 9|signal: 9, SIGKILL'

# A test failure is never the flake.
if grep -qE '(^|[[:space:]])FAIL[[:space:]]|panicked|test result: FAILED' <<<"$clean"; then
  exit 1
fi

# Linker errors count only when each one has a matching SIGKILL note.
# Then cargo's "due to N previous errors" summaries are theirs, not a
# compiler diagnostic's.
links=$(grep -cE "$link_error" <<<"$clean" || true)
killed=$(grep -cE "$link_killed" <<<"$clean" || true)
allowed="$could_not_compile|$cargo_no_run"
if [ "$links" -gt 0 ]; then
  [ "$killed" -ge "$links" ] || exit 1
  allowed="$allowed|$link_error|$link_killed|$due_to_previous"
fi

# Any other compiler diagnostic (error[E....] or error: line) is real.
if grep -E '(^|[[:space:]])error(\[E[0-9]+\])?:' <<<"$clean" \
  | grep -vE "$allowed" \
  | grep -q .; then
  exit 1
fi
grep -qE "$could_not_compile|$due_to_previous" <<<"$clean" && exit 0
grep -qE "$cargo_no_run" <<<"$clean" && exit 0
exit 1
