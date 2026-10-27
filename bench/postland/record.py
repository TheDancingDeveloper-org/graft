#!/usr/bin/env python3
"""Follow one line and append a JSONL observation log.

One poll does two reads:

- ``graft api watch`` from the stored LSN. The request is exactly the CLI's
  input shape (``from_lsn``, ``kinds``, ``limit``, ``wait_ms``). ``repo`` is a
  flag, never a field: the CLI rejects unknown fields.
- ``GET /v1/repos/{repo}/land-checks`` for every landing still pending, from
  the oldest pending position, following ``next`` until the page ends or
  ``--pending-limit`` rows have been read. The bearer token is read from
  ``--token-file`` and sent as a header. It is never an argument.

Rows carry the observer's wall clock. Land time is the journal entry's
``at`` when the body has one, otherwise the first time this recorder saw the
entry. Settle time is the first poll that reads the landing as ``good``,
``bad`` or ``reverted``. The gap between them is at most one ``--interval``,
so the poll interval bounds the precision.

``late`` means the entry was first observed more than ``--late-after``
seconds (default 120) after the time it was journaled. A body with no
journal time cannot be late, including the first entry seen after a restart.
Restarting resumes from the stored LSN and appends no LSN twice.

The server's check states are ``pending``, ``good``, ``bad`` and
``reverted``. Blocked time comes only from a journaled attester report
(``AttesterBlocked``); without one it is reported as not observable.

No sleep and no network live in this module. ``--graft`` and ``--http`` name
the commands, so tests replay recorded responses.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

TRACKED = ("RefMoved", "Attested", "LandReverted", "AttesterBlocked")
SETTLED = ("good", "bad", "reverted")
DEFAULT_LATE_AFTER_S = 120


def load_state(path: Path) -> dict:
    if not path.exists():
        return {"lsn": 0, "seen": [], "pending": {}}
    return json.loads(path.read_text(encoding="utf-8"))


def save_state(path: Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(state, sort_keys=True) + "\n", encoding="utf-8")
    tmp.replace(path)


def now_utc() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_time(text: str) -> datetime:
    return datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


def watch(graft: str, repo: str, from_lsn: int, limit: int) -> dict:
    """One page of the journal. ``repo`` is a flag; the body has no repo field."""
    request = {"from_lsn": from_lsn, "kinds": list(TRACKED), "limit": limit, "wait_ms": 0}
    proc = subprocess.run(
        [graft, "api", "watch", "--repo", repo, "--input", "-"],
        input=json.dumps(request),
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"graft api watch exited {proc.returncode}: {proc.stderr.strip()}")
    return json.loads(proc.stdout or "{}")


def land_checks(http: str, endpoint: str, token_file: str, repo: str, line: str, from_pos: int, limit: int, page_token: str | None) -> dict:
    """One page of ``GET /v1/repos/{repo}/land-checks``. The token rides a header."""
    query = f"line={line}&from_pos={from_pos}&limit={limit}"
    if page_token:
        query += f"&page_token={page_token}"
    url = f"{endpoint}/v1/repos/{repo}/land-checks?{query}"
    proc = subprocess.run(
        [http, "-sS", "-m", "30", "--header", "@" + token_file, url],
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"land-checks exited {proc.returncode}: {proc.stderr.strip()}")
    return json.loads(proc.stdout or "{}")


def token_header_file(token_file: Path, dest: Path) -> Path:
    """Write ``Authorization: Bearer <token>`` for curl's ``--header @file``.

    The token is read from the file and written to another file. It never
    appears in a command argument.
    """
    token = token_file.read_text(encoding="utf-8").strip()
    dest.write_text(f"Authorization: Bearer {token}\n", encoding="utf-8")
    return dest


def is_late(body: dict, observed_at: str, late_after_s: int) -> bool:
    """Late only when the body names the time it was journaled and observation lags it."""
    journaled = body.get("at")
    if not journaled:
        return False
    lag = (parse_time(observed_at) - parse_time(journaled)).total_seconds()
    return lag > late_after_s


def observe(entries: list[dict], *, now: str, late_after_s: int) -> list[dict]:
    rows = []
    for entry in entries:
        kind = entry.get("kind")
        if kind not in TRACKED:
            continue
        body = json.loads(entry.get("body") or "{}")
        row = {
            "lsn": int(entry["lsn"]),
            "kind": kind,
            "ref": body.get("name"),
            "pos": body.get("pos"),
            "changeset": body.get("new") or body.get("land") or body.get("changeset"),
            "journaled_at": body.get("at"),
            "observed_at": now,
            "late": is_late(body, now, late_after_s),
        }
        if kind == "LandReverted":
            row["revert"] = body.get("revert")
            row["actor"] = body.get("actor")
        if kind == "Attested":
            row["attest_kind"] = body.get("kind")
        if kind == "AttesterBlocked":
            row["blocked"] = True
            row["changeset"] = body.get("changeset")
        rows.append(row)
    return rows


def append_rows(path: Path, rows: list[dict], seen: set[int]) -> list[dict]:
    fresh = [row for row in rows if row["lsn"] not in seen]
    if not fresh:
        return []
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        for row in fresh:
            handle.write(json.dumps(row, sort_keys=True) + "\n")
            seen.add(row["lsn"])
    return fresh


def pending_lands(state: dict) -> dict:
    """Lands observed and not yet settled, keyed by changeset."""
    return state.setdefault("pending", {})


def apply_checks(state: dict, rows: list[dict], *, now: str) -> list[dict]:
    """Mark pending lands from a land-checks page. Returns new observation rows."""
    pending = pending_lands(state)
    by_cs = {row.get("changeset"): row for row in rows}
    stamped = []
    for changeset, land in list(pending.items()):
        row = by_cs.get(changeset)
        if row is None or row.get("state") not in SETTLED:
            continue
        pos = int(row["pos"])
        state_name = row["state"]
        stamped.append(
            {
                "lsn": -(pos * 4 + {"good": 1, "bad": 2, "reverted": 3}[state_name]),
                "kind": "LandCheck",
                "pos": pos,
                "changeset": changeset,
                "state": state_name,
                "revert": row.get("revert"),
                "journaled_at": land.get("journaled_at"),
                "landed_observed_at": land.get("observed_at"),
                "observed_at": now,
                "late": False,
            }
        )
        del pending[changeset]
    return stamped


def run_once(*, graft: str, http: str, endpoint: str, token_file: Path, repo: str, line: str, state_path: Path, out_path: Path, now: str, page_limit: int, pending_limit: int, late_after_s: int) -> int:
    state = load_state(state_path)
    stored = int(state.get("lsn", 0))
    seen = set(state.get("seen", []))
    page = watch(graft, repo, stored, page_limit)
    rows = observe(page.get("entries", []), now=now, late_after_s=late_after_s)
    written = append_rows(out_path, rows, seen)
    for row in rows:
        if row["kind"] == "RefMoved" and row.get("ref") == line and row["changeset"] not in pending_lands(state):
            pending_lands(state)[row["changeset"]] = {"observed_at": row["observed_at"], "journaled_at": row["journaled_at"]}
    if pending_lands(state):
        header = token_header_file(token_file, state_path.with_suffix(".header"))
        read = 0
        token = None
        collected: list[dict] = []
        while read < pending_limit:
            checks = land_checks(http, endpoint, str(header), repo, line, 0, page_limit, token)
            batch = checks.get("rows", [])
            collected.extend(batch)
            read += len(batch)
            token = checks.get("next")
            if not token or not batch:
                break
        written += append_rows(out_path, apply_checks(state, collected, now=now), seen)
    state["lsn"] = max(stored, int(page.get("next_lsn", stored)))
    state["seen"] = sorted(seen)
    save_state(state_path, state)
    return len(written)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--line", required=True)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--graft", default="graft")
    parser.add_argument("--http", default="curl", help="HTTP client. Tests pass a stub.")
    parser.add_argument("--endpoint", default="http://graft-dev:17888")
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--now", default=None)
    parser.add_argument("--limit", type=int, default=200)
    parser.add_argument("--pending-limit", type=int, default=500, help="Most pending land-check rows read per poll.")
    parser.add_argument("--interval", type=int, default=60, help="Seconds between polls. Bounds M1 precision.")
    parser.add_argument("--late-after", type=int, default=DEFAULT_LATE_AFTER_S, help="Seconds after journal time before an entry is late.")
    parser.add_argument("--once", action="store_true", help="One poll, then exit. The default is also one poll.")
    args = parser.parse_args(argv)
    count = run_once(
        graft=args.graft,
        http=args.http,
        endpoint=args.endpoint,
        token_file=args.token_file,
        repo=args.repo,
        line=args.line,
        state_path=args.state,
        out_path=args.out,
        now=args.now or now_utc(),
        page_limit=args.limit,
        pending_limit=args.pending_limit,
        late_after_s=args.late_after,
    )
    print(f"recorded {count} interval={args.interval}s late_after={args.late_after}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
