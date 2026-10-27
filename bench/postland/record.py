#!/usr/bin/env python3
"""Follow one post-land line and append a JSONL observation log.

One poll does two reads, in this order:

1. ``graft api watch`` from the stored LSN, page after page until
   ``next_lsn`` stops moving. The request is the CLI's input shape
   (``from_lsn``, ``kinds``, ``limit``, ``wait_ms``); ``--repo`` and
   ``--token-file`` are flags, so the token path is an argument and the token
   is not. ``RefMoved`` and ``LandReverted`` rows are kept for ``--line``;
   every ``Attested`` row is kept (M3 joins them to the line's lands).
2. ``GET /v1/repos/{repo}/land-checks`` from the oldest position still
   unsettled, or from the frontier (one past the highest position read so
   far) when nothing is unsettled, following ``next`` to the end. Both
   positions live in the state file, so the cost of a poll follows the
   unsettled set and the new lands, never the line's history. The bearer
   token is read from ``--token-file`` and sent as a request header. It is
   never an argument and never written to disk.

What the server sends, and what it does not:

- ``RefMoved`` is ``{name, old, new, version, actor}``. ``LandReverted`` is
  ``{line, land, revert, check}``. Neither carries a time or a position.
  A land-checks row is ``{pos, changeset, state, revert}`` with states
  ``pending``, ``good``, ``bad`` and ``reverted``. Nothing journals a line
  being blocked.
- So every time in the log is this recorder's own clock (``observed_at``).
  A land's time is its first observation by either read, and its settle time
  is the first poll that reads it ``good``, ``bad`` or ``reverted``. Each is
  late by at most one ``--interval``; that bounds the precision.

``late`` is about the observer, not the server. A row is late when its poll
came more than ``--late-after`` seconds (default two intervals) after the
previous committed poll, or when there was no previous poll: what it reports
may have happened at any time in that gap. A ``Gap`` row records each such
gap. The report excludes late samples from the percentiles and counts them.

Restart safety: the state file stores the log's length after the last
committed poll. A restart truncates anything after that length (a poll that
died between the log and the state write) and redoes the poll, so no row is
written twice.

``--graft`` names the CLI and ``fetch_json`` is the only HTTP call, so tests
replay recorded pages. Nothing here sleeps except ``--loop``.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import quote, urlencode

KINDS = ("RefMoved", "Attested", "LandReverted")
TERMINAL = ("good", "reverted")
SETTLED = ("good", "bad", "reverted")


def now_utc() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_time(text: str) -> datetime:
    return datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


def load_state(path: Path, *, from_lsn: int | None, from_pos: int | None) -> dict:
    if path.exists():
        return json.loads(path.read_text(encoding="utf-8"))
    if from_lsn is None or from_pos is None:
        raise SystemExit("no state yet: pass --from-lsn and --from-pos for the first poll")
    return {"lsn": from_lsn, "frontier": from_pos, "pending": {}, "out_bytes": 0, "last_poll_at": None}


def save_state(path: Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as handle:
        handle.write(json.dumps(state, sort_keys=True) + "\n")
        handle.flush()
        os.fsync(handle.fileno())
    tmp.replace(path)


def reconcile_log(out: Path, committed: int) -> None:
    """Drop rows of a poll that never committed its state."""
    size = out.stat().st_size if out.exists() else 0
    if size < committed:
        raise SystemExit(f"{out} is shorter than the state says ({size} < {committed}); refusing to guess")
    if size > committed:
        with out.open("r+b") as handle:
            handle.truncate(committed)


def append_rows(out: Path, rows: list[dict]) -> int:
    """Append rows durably. Returns the log's new length."""
    out.parent.mkdir(parents=True, exist_ok=True)
    with out.open("a", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, sort_keys=True) + "\n")
        handle.flush()
        os.fsync(handle.fileno())
    return out.stat().st_size


def watch(graft: str, repo: str, token_file: Path, from_lsn: int, limit: int) -> dict:
    """One page of the journal through ``graft api watch``."""
    request = {"from_lsn": from_lsn, "kinds": list(KINDS), "limit": limit, "wait_ms": 0}
    proc = subprocess.run(
        [graft, "api", "watch", "--repo", repo, "--token-file", str(token_file), "--input", "-"],
        input=json.dumps(request),
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"graft api watch exited {proc.returncode}: {proc.stderr.strip()}")
    return json.loads(proc.stdout)


def fetch_json(url: str, headers: dict[str, str]) -> dict:
    """GET one JSON document. Errors name the status, never the headers."""
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as err:
        raise RuntimeError(f"GET {url.split('?')[0]} answered {err.code}") from None


def land_checks(endpoint: str, token_file: Path, repo: str, line: str, from_pos: int, limit: int, page_token: str | None) -> dict:
    """One page of ``GET /v1/repos/{repo}/land-checks``."""
    query = {"line": line, "from_pos": from_pos, "limit": limit}
    if page_token:
        query["page_token"] = page_token
    url = f"{endpoint.rstrip('/')}/v1/repos/{quote(repo, safe='')}/land-checks?{urlencode(query)}"
    token = token_file.read_text(encoding="utf-8").strip()
    return fetch_json(url, {"Authorization": f"Bearer {token}"})


def journal_row(entry: dict, line: str) -> dict | None:
    """The log row for one journal entry, or None when it is off the line."""
    kind = entry["kind"]
    body = json.loads(entry["body"])
    row = {"src": "journal", "lsn": int(entry["lsn"]), "kind": kind}
    if kind == "RefMoved":
        if body.get("name") != line:
            return None
        row.update(changeset=body["new"], old=body.get("old"), actor=body.get("actor"))
    elif kind == "LandReverted":
        if body.get("line") != line:
            return None
        row.update(changeset=body["land"], revert=body["revert"], check=body.get("check"))
    elif kind == "Attested":
        # The v0.1 body has no kind and is a check; it carries `passed`.
        status = body.get("status") or ("pass" if body.get("passed") else "fail")
        row.update(changeset=body["changeset"], check=body.get("check"), attest_kind=body.get("kind", "check"), status=status, actor=body.get("actor"))
    else:
        return None
    return row


def read_journal(graft: str, repo: str, token_file: Path, line: str, from_lsn: int, limit: int) -> tuple[list[dict], int]:
    """Every tracked entry from ``from_lsn`` to the head, and the next LSN."""
    rows: list[dict] = []
    lsn = from_lsn
    while True:
        page = watch(graft, repo, token_file, lsn, limit)
        for entry in page.get("entries", []):
            row = journal_row(entry, line)
            if row is not None:
                rows.append(row)
        nxt = int(page.get("next_lsn", lsn))
        if nxt <= lsn:
            return rows, lsn
        lsn = nxt


def checks_start(state: dict) -> int:
    """The oldest unsettled position, or the frontier when none is unsettled."""
    return min([int(p["pos"]) for p in state["pending"].values()] + [int(state["frontier"])])


def apply_checks(state: dict, rows: list[dict]) -> list[dict]:
    """Fold land-checks rows into the state. Returns one log row per change seen.

    A position at or past the frontier is a land not read before: it is
    logged whatever its state. Below the frontier only unsettled lands are
    followed, and a row is logged only when the state moved. ``good`` and
    ``reverted`` end the following; ``bad`` waits for its revert.
    """
    frontier = int(state["frontier"])
    pending = state["pending"]
    out = []
    for row in rows:
        pos, changeset, check_state = int(row["pos"]), row["changeset"], row["state"]
        known = pending.get(changeset)
        if pos < frontier and (known is None or known["state"] == check_state):
            continue
        out.append({"src": "land-checks", "kind": "LandCheck", "pos": pos, "changeset": changeset, "state": check_state, "revert": row.get("revert")})
        if check_state in TERMINAL:
            pending.pop(changeset, None)
        else:
            pending[changeset] = {"pos": pos, "state": check_state}
    if rows:
        state["frontier"] = max(frontier, max(int(r["pos"]) for r in rows) + 1)
    return out


def read_checks(endpoint: str, token_file: Path, repo: str, line: str, from_pos: int, limit: int) -> list[dict]:
    """Every land-checks row from ``from_pos`` to the end of the line."""
    rows: list[dict] = []
    token = None
    while True:
        page = land_checks(endpoint, token_file, repo, line, from_pos, limit, token)
        rows.extend(page.get("rows", []))
        token = page.get("next")
        if not token:
            return rows


def run_once(*, graft: str, endpoint: str, token_file: Path, repo: str, line: str, state_path: Path, out_path: Path, now: str, watch_limit: int, checks_limit: int, late_after_s: int, from_lsn: int | None = None, from_pos: int | None = None) -> int:
    """One poll. Returns the number of rows written."""
    state = load_state(state_path, from_lsn=from_lsn, from_pos=from_pos)
    reconcile_log(out_path, int(state["out_bytes"]))
    last = state.get("last_poll_at")
    gap_s = None if last is None else (parse_time(now) - parse_time(last)).total_seconds()
    late = gap_s is None or gap_s > late_after_s

    journal, next_lsn = read_journal(graft, repo, token_file, line, int(state["lsn"]), watch_limit)
    checks = read_checks(endpoint, token_file, repo, line, checks_start(state), checks_limit)
    rows = []
    if late and last is not None:
        rows.append({"kind": "Gap", "from": last, "to": now, "seconds": gap_s})
    for row in journal + apply_checks(state, checks):
        if row["kind"] != "Gap":
            row.update(observed_at=now, late=late)
        rows.append(row)

    state["out_bytes"] = append_rows(out_path, rows) if rows else int(state["out_bytes"])
    state["lsn"] = next_lsn
    state["last_poll_at"] = now
    save_state(state_path, state)
    return len(rows)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--line", required=True)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--token-file", type=Path, required=True, help="Read-only graft token. Read each poll; never printed.")
    parser.add_argument("--graft", default="graft")
    parser.add_argument("--endpoint", default="http://graft-dev:17888")
    parser.add_argument("--from-lsn", type=int, default=None, help="First poll only: the LSN to start from.")
    parser.add_argument("--from-pos", type=int, default=None, help="First poll only: the line position to start from (0 for a line just switched to post-land).")
    parser.add_argument("--watch-limit", type=int, default=500)
    parser.add_argument("--checks-limit", type=int, default=100)
    parser.add_argument("--interval", type=int, default=60, help="Seconds between polls with --loop. Bounds M1 precision.")
    parser.add_argument("--late-after", type=int, default=None, help="Gap in seconds after which a poll's rows are late (default 2 x --interval).")
    parser.add_argument("--now", default=None, help="Observer time for a single poll (tests).")
    parser.add_argument("--loop", action="store_true", help="Poll every --interval seconds until stopped.")
    args = parser.parse_args(argv)
    late_after = args.late_after if args.late_after is not None else 2 * args.interval
    while True:
        try:
            count = run_once(
                graft=args.graft, endpoint=args.endpoint, token_file=args.token_file, repo=args.repo, line=args.line,
                state_path=args.state, out_path=args.out, now=args.now or now_utc(), watch_limit=args.watch_limit,
                checks_limit=args.checks_limit, late_after_s=late_after, from_lsn=args.from_lsn, from_pos=args.from_pos,
            )
            print(f"recorded {count}", flush=True)
        except RuntimeError as err:
            if not args.loop:
                raise
            # The next committed poll records the gap.
            print(f"poll failed: {err}", file=sys.stderr, flush=True)
        if not args.loop:
            return 0
        time.sleep(args.interval)


if __name__ == "__main__":
    sys.exit(main())
