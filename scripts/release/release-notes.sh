#!/bin/sh
# Extract one committed CHANGELOG.md section for GitHub release notes.
# Usage: release-notes.sh <version> <output>
set -eu

version=${1:?usage: release-notes.sh <version> <output>}
output=${2:?usage: release-notes.sh <version> <output>}
case $version in
  v*) version=${version#v} ;;
esac

root=$(cd "$(dirname -- "$0")/../.." && pwd)
changelog=$root/CHANGELOG.md

[ -f "$changelog" ] || {
  printf 'missing %s\n' "$changelog" >&2
  exit 1
}

awk -v version="$version" '
  $1 == "##" && $2 == version { found = 1 }
  found && $1 == "##" && $2 != version { exit }
  found { print }
  END { if (!found) exit 1 }
' "$changelog" > "$output"

[ -s "$output" ] || {
  printf 'empty changelog section for %s\n' "$version" >&2
  exit 1
}
