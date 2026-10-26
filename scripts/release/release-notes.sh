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

# GitHub rejects a release body over 125,000 characters. Keep the notes
# under 100,000 bytes by cutting before the last "### " section line that
# still fits, then point at the full changelog.
limit=100000
size=$(wc -c < "$output")
if [ "$size" -gt "$limit" ]; then
  repo=${GITHUB_REPOSITORY:-TheDancingDeveloper-org/graft}
  footer="The rest of the notes for v$version are in [CHANGELOG.md](https://github.com/$repo/blob/v$version/CHANGELOG.md)."
  budget=$((limit - ${#footer} - 2))
  LC_ALL=C awk -v budget="$budget" '
    { lines[NR] = $0; off[NR] = total; total += length($0) + 1 }
    /^### / && off[NR] <= budget { cut = NR }
    END {
      if (!cut) exit 1
      for (i = 1; i < cut; i++) print lines[i]
    }
  ' "$output" > "$output.tmp" || {
    rm -f "$output.tmp"
    printf 'no section boundary under %s bytes for %s\n' "$limit" "$version" >&2
    exit 1
  }
  printf '\n%s\n' "$footer" >> "$output.tmp"
  mv "$output.tmp" "$output"
fi
