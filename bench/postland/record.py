#!/usr/bin/env python3
"""Follow one line and append a JSONL observation log.

`record.py` reads the journal from a stored LSN through `graft api watch`
and reads `GetLandChecks` for landings still pending. Every row carries the
observer's own wall clock. A restart resumes from the stored LSN and writes
no row twice. A journal entry whose LSN is older than the LSN already stored
when this process started is flagged `late`: it arrived after a recorder gap,
is excluded from latency, and still counts for coverage.

No network and no sleep live in this module. `graft api` is a command the
caller supplies (`--graft`, default `graft`), so tests feed a recorded journal
fixture through a stub.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

TRACKED = ("RefMoved", "Attested", "LandReverted")


def load_state(path: Path) -> dict:
    if not path.exists():
        return {"lsn": 0, "seen": []}
    return json.loads(path.read_text(encoding="utf-8"))


def save_state(path: Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(state, sort_keys=True) + "\n", encoding="utf-8")
    tmp.replace(path)


def graft_api(graft: str, op: str, request: dict, repo: str) -> dict:
    proc = subprocess.run(
        [graft, "api", op, "--repo", repo, "--input", "-"],
        input=json.dumps(request),
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"graft api {op} exited {proc.returncode}: {proc.stderr.strip()}")
    return json.loads(proc.stdout or "{}")


def _pos(body: dict) -> int | None:
    for key in ("pos", "position"):
        if key in body:
            return int(body[key])
    return None


def observe(entries: list[dict], *, stored_lsn: int, now: str) -> list[dict]:
    """Turn journal entries into JSONL rows.

    `stored_lsn` is the LSN already on disk when this process started. An
    entry at or below it is `late`.
    """
    rows = []
    for entry in entries:
        kind = entry.get("kind")
        if kind not in TRACKED:
            continue
        body = json.loads(entry.get("body") or "{}")
        lsn = int(entry["lsn"])
        row = {
            "lsn": lsn,
            "kind": kind,
            "pos": _pos(body),
            "changeset": body.get("changeset") or body.get("land"),
            "observed_at": now,
            "late": lsn <= stored_lsn,
        }
        if kind == "LandReverted":
            row["revert"] = body.get("revert")
        rows.append(row)
    return rows


def append_rows(path: Path, rows: list[dict], seen: set[int]) -> list[dict]:
    """Append rows whose LSN is not already recorded. Returns the new rows."""
    fresh = [row for row in rows if row["lsn"] not in seen]
    if not fresh:
        return []
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        for row in fresh:
            handle.write(json.dumps(row, sort_keys=True) + "\n")
            seen.add(row["lsn"])
    return fresh


def run_once(*, graft: str, repo: str, line: str, state_path: Path, out_path: Path, now: str, page_limit: int) -> int:
    state = load_state(state_path)
    stored = int(state.get("lsn", 0))
    seen = set(state.get("seen", []))
    page = graft_api(
        graft,
        "watch",
        {"repo": repo, "from_lsn": stored, "kinds": list(TRACKED), "limit": page_limit, "wait_ms": 0},
        repo,
    )
    rows = observe(page.get("entries", []), stored_lsn=stored, now=now)
    written = append_rows(out_path, rows, seen)
    next_lsn = int(page.get("next_lsn", stored))
    pending = [row for row in rows if row["kind"] == "RefMoved"]
    if pending:
        oldest = min(row["pos"] for row in pending if row["pos"] is not None)
        checks = graft_api(
            graft,
            "get_land_checks",
            {"repo": repo, "line": line, "from_pos": oldest, "page": {"limit": page_limit, "token": None}},
            repo,
        )
        stamped = []
        for item in checks.get("rows", []):
            if item.get("state") not in ("good", "bad", "reverted"):
                continue
            pos = int(item["pos"])
            state_name = item["state"]
            # Negative synthetic id: journal LSNs are positive, so a check
            # stamp can never collide with one, and the same stamp is one row.
            row = {
                "lsn": -(pos * 4 + {"good": 1, "bad": 2, "reverted": 3}[state_name]),
                "kind": "LandCheck",
                "pos": pos,
                "changeset": item.get("changeset"),
                "state": state_name,
                "revert": item.get("revert"),
                "observed_at": now,
                "late": False,
            }
            stamped.append(row)
        written += append_rows(out_path, stamped, seen)
    state["lsn"] = max(stored, next_lsn)
    state["seen"] = sorted(seen)
    save_state(state_path, state)
    return len(written)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--line", required=True)
    parser.add_argument("--state", type=Path, required=True, help="Resume cursor (JSON)")
    parser.add_argument("--out", type=Path, required=True, help="JSONL log")
    parser.add_argument("--graft", default="graft")
    parser.add_argument("--now", default=None, help="Observer timestamp (default: UTC now)")
    parser.add_argument("--limit", type=int, default=200)
    args = parser.parse_args(argv)
    from datetime import datetime, timezone

    now = args.now or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    count = run_once(
        graft=args.graft,
        repo=args.repo,
        line=args.line,
        state_path=args.state,
        out_path=args.out,
        now=now,
        page_limit=args.limit,
    )
    print(f"recorded {count}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
