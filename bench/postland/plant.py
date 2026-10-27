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

Each land runs ``graft land --json`` and takes the id from ``landed``. A
contended land (exit 4) is retried up to three times. ``--record FILE``
appends ``{"plant", "follower", "variant", "landed_at"}`` for ``report.py
--plants``. ``--graft`` names the CLI, so tests pass a stub.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
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
CONTENDED = 4


def plant_body(variant: str) -> str:
    if variant == "fmt":
        return FMT_BODY
    if variant == "test":
        return TEST_BODY
    raise SystemExit(f"unknown variant {variant!r}")


def follower_body(variant: str) -> str:
    return plant_body(variant) + FOLLOWER


def _run(argv: list[str], env: dict[str, str], cwd: Path) -> subprocess.CompletedProcess:
    return subprocess.run(argv, text=True, capture_output=True, check=False, env=env, cwd=cwd)


def _check(proc: subprocess.CompletedProcess, what: str) -> str:
    if proc.returncode != 0:
        raise RuntimeError(f"graft {what} exited {proc.returncode}: {proc.stderr.strip()}")
    return proc.stdout


def commit_and_land(graft: str, message: str, env: dict[str, str], cwd: Path) -> str:
    """Commit the working copy, land it, return the landed changeset id."""
    _check(_run([graft, "commit", "-m", message, "--meta", f"agent.work_item={PLANT_WORK_ITEM}"], env, cwd), "commit")
    for _ in range(3):
        proc = _run([graft, "land", "--json"], env, cwd)
        if proc.returncode != CONTENDED:
            break
    out = json.loads(_check(proc, "land"))
    if out.get("outcome") != "landed" or not out.get("landed"):
        raise RuntimeError(f"graft land did not land: outcome {out.get('outcome')!r}")
    return out["landed"]


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
    plant_id = commit_and_land(graft, f"postland plant {variant}", base, root)
    target.write_text(follower_body(variant), encoding="utf-8")
    follower_id = commit_and_land(graft, f"postland follower {variant}", base, root)
    return plant_id, follower_id


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", required=True, choices=("fmt", "test"))
    parser.add_argument("--root", type=Path, default=Path("."))
    parser.add_argument("--graft", default="graft")
    parser.add_argument("--record", type=Path, default=None, help="Append the pair as one JSON line, for report.py --plants.")
    args = parser.parse_args(argv)
    plant_id, follower_id = land_pair(root=args.root, variant=args.variant, graft=args.graft)
    if args.record:
        landed_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        with args.record.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps({"plant": plant_id, "follower": follower_id, "variant": args.variant, "landed_at": landed_at}) + "\n")
    print(plant_id)
    print(follower_id)
    return 0


if __name__ == "__main__":
    sys.exit(main())
