#!/usr/bin/env bash
# Fail if the user guide names estate hosts, trackers, or internal ids.
# Run by hand. Not part of CI, and not part of graft's land path.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
shopt -s nullglob
files=(docs/guide/* docs/agents.md)
pattern='graft-dev|node-b|hetzner|hertz|proxmox|edgerouter|komodo|cadastre|infisical|winrarhost|vogt|trial-[0-9]|rusttorrent|\bWI-[0-9]+|\bD[0-9]{2}\b|\bG[0-9]\.[0-9]+'
if grep -nEiH "$pattern" "${files[@]}"; then
  exit 1
fi
