#!/usr/bin/env bash
# Fail if the user guide names estate hosts, trackers, or internal ids.
# Runs in repository CI on the docs-only path. Never in graft's land path.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
shopt -s nullglob
files=(docs/guide/* docs/agents.md)
pattern='graft-dev|node-b|hetzner|hertz|proxmox|edgerouter|komodo|cadastre|infisical|winrarhost|vogt|trial-[0-9]|rusttorrent|\bWI-[0-9]+|\bD[0-9]{2}\b|\bG[0-9]\.[0-9]+'
if grep -nEiH "$pattern" "${files[@]}"; then
  exit 1
fi

# Relative links in the user guide must resolve to a file or directory.
python3 - << 'PY'
import re
import sys
from pathlib import Path

root = Path("docs/guide")
missing = []
link = re.compile(r"\[[^\]]*\]\(([^)]+)\)")
for path in sorted(root.glob("*.md")):
    for match in link.finditer(path.read_text()):
        target = match.group(1).split("#", 1)[0].strip()
        if not target or "://" in target or target.startswith("mailto:"):
            continue
        resolved = (path.parent / target).resolve()
        try:
            resolved.relative_to(Path.cwd().resolve())
        except ValueError:
            missing.append(f"{path}: {target} escapes the repository")
            continue
        if not resolved.exists():
            missing.append(f"{path}: {target}")
if missing:
    print("broken relative links:", file=sys.stderr)
    print("\n".join(missing), file=sys.stderr)
    sys.exit(1)
PY
