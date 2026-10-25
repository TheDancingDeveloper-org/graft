#!/bin/sh
# Build the release binaries for one target and tar them.
# Usage: build-archive.sh <rust-target> <cross|cargo>
# Binary list: scripts/release/binaries.txt (package:executable).
set -eu

target=${1:?target}
tool=${2:?cross or cargo}
root=$(cd "$(dirname -- "$0")/../.." && pwd)
cd "$root"

mkdir -p dist
while IFS=: read -r package bin; do
  case $package in
    ''|\#*) continue ;;
  esac
  package=$(printf '%s' "$package" | tr -d '[:space:]')
  bin=$(printf '%s' "$bin" | tr -d '[:space:]')
  if [ "$tool" = cross ]; then
    cross build --locked --release --target "$target" -p "$package"
  else
    cargo build --locked --release --target "$target" -p "$package"
  fi
  install -m 0755 "target/$target/release/$bin" "dist/$bin"
done < scripts/release/binaries.txt

archive="graft-${target}.tar.gz"
tar -C dist -czf "$archive" .
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$archive" > "${archive}.sha256"
else
  shasum -a 256 "$archive" > "${archive}.sha256"
fi
printf 'built %s\n' "$archive"
