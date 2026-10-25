#!/bin/sh
# Assemble CHANGELOG.md from changes/*.md fragments.
#
# Usage: scripts/release/changelog.sh <version>
#   version is the release tag without a leading v (for example 0.1.0).
#
# Fragments are concatenated under a dated heading, then deleted so the
# release PR can commit CHANGELOG.md and the consumed fragments together.
# changes/README.md is retained.
set -eu

version=${1:?usage: changelog.sh <version>}
case $version in
  v*) version=${version#v} ;;
esac

root=$(cd "$(dirname -- "$0")/../.." && pwd)
changes=$root/changes
out=$root/CHANGELOG.md
date=$(date -u +%Y-%m-%d)

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

{
  printf '## %s — %s\n\n' "$version" "$date"
  found=0
  for fragment in "$changes"/*.md; do
    [ -f "$fragment" ] || continue
    name=$(basename "$fragment")
    [ "$name" = README.md ] && continue
    found=1
    printf '### %s\n\n' "${name%.md}"
    cat "$fragment"
    printf '\n'
  done
  if [ "$found" -eq 0 ]; then
    printf '_No changelog fragments._\n\n'
  fi
  if [ -f "$out" ]; then
    printf '\n'
    cat "$out"
  fi
} > "$tmp"

mv "$tmp" "$out"
trap - EXIT
for fragment in "$changes"/*.md; do
  [ -f "$fragment" ] || continue
  [ "$(basename "$fragment")" = README.md ] && continue
  rm -- "$fragment"
done
printf 'wrote %s\n' "$out"
