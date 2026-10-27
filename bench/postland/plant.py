#!/usr/bin/env python3
"""Land a planted bad change and one good follower on the plant identity.

Two variants, both in ``crates/graft-harness/tests/planted.rs``:

- ``fmt`` writes a misformatted line, so ``ci/fmt`` fails.
- ``test`` writes ``#[test] fn planted() { panic!() }``, so ``ci/test`` fails.

The plant lands first. The follower is a one-line comment appended after it,
so a later skip-ahead fails at the tip and bisect has to find the plant.
Both lands use ``GRAFT_SESSION_ID=postland-plant`` and
``GRAFT_WORK_ITEM=WI-2604``. The changeset id of each land is printed, plant
first.

``--graft`` and ``--commit`` and ``--land`` name the commands. Tests pass a
stub; nothing here sleeps or opens a network connection itself.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

PLANT_SESSION = "postland-plant"
PLANT_WORK_ITEM = "WI-2604"
PLANTED = Path("crates/graft-harness/tests/planted.rs")

FMT_BODY = (
    "pub fn planted() {\n"
    "        let value=1;\n"
    "}\n"
)
TEST_BODY = (
    "#[test]\n"
    "fn planted() {\n"
    "    panic!()\n"
    "}\n"
)
FOLLOWER = "// postland follower: keep the plant off the tip\n"


def plant_body(variant: str) -> str:
    if variant == "fmt":
        return FMT_BODY
    if variant == "test":
        return TEST_BODY
    raise SystemExit(f"unknown variant {variant!r}")


def follower_body(variant: str) -> str:
    return plant_body(variant) + FOLLOWER


def _run(argv: list[str], env: dict[str, str]) -> str:
    proc = subprocess.run(argv, text=True, capture_output=True, check=False, env=env)
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(argv)} exited {proc.returncode}: {proc.stderr.strip()}")
    return proc.stdout.strip()


def land_pair(*, root: Path, variant: str, graft: str, env: dict[str, str] | None = None) -> tuple[str, str]:
    """Write the plant, land it, write the follower, land it. Return both ids."""
    base = dict(os.environ)
    if env:
        base.update(env)
    base["GRAFT_SESSION_ID"] = PLANT_SESSION
    base["GRAFT_WORK_ITEM"] = PLANT_WORK_ITEM
    target = root / PLANTED
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(plant_body(variant), encoding="utf-8")
    _run([graft, "commit", "-m", f"postland plant {variant}", "--meta", f"agent.work_item={PLANT_WORK_ITEM}"], base)
    plant_id = _run([graft, "land"], base).split()[-1]
    target.write_text(follower_body(variant), encoding="utf-8")
    _run([graft, "commit", "-m", f"postland follower {variant}", "--meta", f"agent.work_item={PLANT_WORK_ITEM}"], base)
    follower_id = _run([graft, "land"], base).split()[-1]
    return plant_id, follower_id


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", required=True, choices=("fmt", "test"))
    parser.add_argument("--root", type=Path, default=Path("."))
    parser.add_argument("--graft", default="graft")
    args = parser.parse_args(argv)
    plant_id, follower_id = land_pair(root=args.root, variant=args.variant, graft=args.graft)
    print(plant_id)
    print(follower_id)
    return 0


if __name__ == "__main__":
    sys.exit(main())
