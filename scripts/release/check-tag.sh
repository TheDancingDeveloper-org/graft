#!/bin/sh
# Verify that a release tag matches the workspace version and has release notes.
# Usage: scripts/release/check-tag.sh <tag>
set -eu

tag=${1:?usage: check-tag.sh <tag>}
case $tag in
  v*) version=${tag#v} ;;
  *)
    printf 'release tag must start with v: %s\n' "$tag" >&2
    exit 1
    ;;
esac

root=$(cd "$(dirname -- "$0")/../.." && pwd)
workspace_version=$(
  awk '
    $0 == "[workspace.package]" { workspace_package = 1; next }
    workspace_package && /^\[/ { exit }
    workspace_package && /^[[:space:]]*version[[:space:]]*=/ {
      value = $0
      sub(/^[^=]*=[[:space:]]*"/, "", value)
      sub(/"[[:space:]]*$/, "", value)
      print value
      exit
    }
  ' "$root/Cargo.toml"
)

[ -n "$workspace_version" ] || {
  printf 'workspace package version is missing from %s/Cargo.toml\n' "$root" >&2
  exit 1
}

[ "$version" = "$workspace_version" ] || {
  printf 'release tag %s does not match workspace version %s\n' \
    "$tag" "$workspace_version" >&2
  exit 1
}

notes=$(mktemp)
trap 'rm -f "$notes"' EXIT
"$root/scripts/release/release-notes.sh" "$version" "$notes"
printf 'release tag %s matches the workspace version and changelog\n' "$tag"
