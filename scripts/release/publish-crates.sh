#!/bin/sh
# Publish workspace crates whose names start with graft-.
# Never publish a crate named exactly `graft`.
#
# Default is cargo publish --dry-run. Set RELEASE_PUBLISH=1 for a real
# upload. Cargo reads CARGO_REGISTRY_TOKEN itself; this script never prints it.
set -eu

root=$(cd "$(dirname -- "$0")/../.." && pwd)
cd "$root"

dry=--dry-run
if [ "${RELEASE_PUBLISH:-0}" = 1 ]; then
  dry=
fi

cargo metadata --no-deps --format-version 1 \
  | python3 -c '
import json, sys
meta = json.load(sys.stdin)
for pkg in meta["packages"]:
    name = pkg["name"]
    if name == "graft" or not name.startswith("graft-"):
        print("skip " + name, file=sys.stderr)
        continue
    # cargo metadata: publish = false becomes an empty list.
    if pkg.get("publish") == []:
        print("skip-unpublished " + name, file=sys.stderr)
        continue
    print(name)
' \
  | while IFS= read -r name; do
      [ -n "$name" ] || continue
      echo "cargo publish ${dry:-<live>} -p $name"
      # shellcheck disable=SC2086
      cargo publish $dry -p "$name"
    done
