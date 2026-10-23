#!/usr/bin/env bash
# Build graft-cli (release) from origin/main and install it to ~/.local/bin/graft.
# Idempotent: when the installed binary was already built from the current
# origin/main sha, it does nothing. Needs git and a Rust toolchain.
#
#   GRAFT_REPO_URL   repository to build (default: the graft GitHub repo)
#   GRAFT_SRC_DIR    where the source checkout lives (default ~/.cache/graft-cli-src)
#   GRAFT_BIN_DIR    install directory (default ~/.local/bin)
set -euo pipefail

url=${GRAFT_REPO_URL:-https://github.com/TheDancingDeveloper-org/graft.git}
src=${GRAFT_SRC_DIR:-$HOME/.cache/graft-cli-src}
bin_dir=${GRAFT_BIN_DIR:-$HOME/.local/bin}
dest=$bin_dir/graft
stamp=$dest.sha

log() { printf 'install-cli: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

command -v git >/dev/null || die "git is required"
command -v cargo >/dev/null || die "cargo is required (install rustup)"

if [ ! -d "$src/.git" ]; then
  mkdir -p "$(dirname "$src")"
  git clone --quiet --no-checkout "$url" "$src"
fi
git -C "$src" fetch --quiet origin main
sha=$(git -C "$src" rev-parse origin/main)

if [ -x "$dest" ] && [ -r "$stamp" ] && [ "$(cat "$stamp")" = "$sha" ]; then
  log "graft ${sha:0:12} is already installed at $dest"
  exit 0
fi

log "building graft-cli at ${sha:0:12}"
git -C "$src" checkout --quiet --detach "$sha"
(cd "$src" && cargo build --release --locked -p graft-cli)

mkdir -p "$bin_dir"
# Install through a temp file and rename, so a running graft is not corrupted.
install -m 0755 "$src/target/release/graft" "$dest.new"
mv -f "$dest.new" "$dest"
printf '%s\n' "$sha" >"$stamp"
log "installed $dest (${sha:0:12})"
"$dest" version
