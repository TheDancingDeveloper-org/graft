#!/bin/sh
# Creates the fixture git repository once, then runs the command given.
# The fixture lives in the graft-dev-mirror-fixture volume, so it survives
# restarts. Nothing here is secret.
set -eu

root=/var/lib/graft/mirror/fixtures
bare="$root/graft-fixture.git"

if [ ! -d "$bare" ]; then
    mkdir -p "$root"
    work=$(mktemp -d)
    git -C "$work" init -q -b main
    printf '# graft fixture\n\nA small repository for the dev stack.\n' > "$work/README.md"
    mkdir -p "$work/src"
    printf 'fn main() {\n    println!("graft fixture");\n}\n' > "$work/src/main.rs"
    git -C "$work" add -A
    git -C "$work" -c user.name=graft-dev -c user.email=graft-dev@localhost \
        commit -q -m "fixture: initial commit"
    git clone -q --bare "$work" "$bare"
    rm -rf "$work"
fi

exec "$@"
