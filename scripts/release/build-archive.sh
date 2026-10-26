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

# Man pages and completions come from a host-built graft, not the
# cross-compiled binary (that one cannot run on the build host).
# `graft man` and `graft completions` are the commands on main.
mkdir -p dist/man dist/completions
cargo run --locked -p graft-cli --quiet -- man --out dist/man
for shell in bash zsh fish; do
  cargo run --locked -p graft-cli --quiet -- completions "$shell" > "dist/completions/graft.${shell}"
done
for generated in dist/man/graft.1 dist/man/graft-land.1 \
  dist/completions/graft.bash dist/completions/graft.zsh dist/completions/graft.fish; do
  if [ ! -s "$generated" ]; then
    printf 'empty generated file: %s\n' "$generated" >&2
    exit 1
  fi
done

archive="graft-${target}.tar.gz"
tar -C dist -czf "$archive" .
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$archive" > "${archive}.sha256"
else
  shasum -a 256 "$archive" > "${archive}.sha256"
fi
printf 'built %s\n' "$archive"
