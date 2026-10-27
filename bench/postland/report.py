#!/usr/bin/env python3
"""Render the v0.2 exit tables from the recorder JSONL and Prometheus.

Every number is recomputable from the JSONL, the operator's input files and
the PromQL printed in the report, evaluated at the instants it lists.

M1 (operator decision 3): p95 <= 30 min and max <= 4 h per day, excluding
time the line was held Blocked for the operator.

- Land time is the recorder's first observation of the land, by either
  read (a ``RefMoved`` for the line, or its first ``LandCheck`` row). The
  server journals no time, so there is nothing earlier to use.
- Settle time is the first ``LandCheck`` row in state ``good``, ``bad`` or
  ``reverted``. Both times are poll times, so each sample is exact to one
  ``--interval``.
- A sample whose land or settle observation is ``late`` (first poll after a
  recorder gap) is left out of the percentiles and counted under ``late``.
- ``pending`` counts lands of the day not settled by the end of that day
  (for the whole window: not settled at all).
- Blocked: the server records no blocked state, so the windows come from
  ``--blocked`` (the operator's list, taken from the checker's ``line ...
  blocked at land ...`` log line and the unblock time). Overlap with a
  window is subtracted from a sample and reported in ``blocked_s``. With no
  file the column reads ``not recorded``.

M2: for each plant (``plant.py --record``), the ``LandReverted`` that names
it, the actor of the ``RefMoved`` that landed the revert, the last check
state of both, and the batch PRs (``--batches``) that contain it.

M3: per day, the line's lands against the ``review`` attestations on them.

D31: the PromQL below, run as instant queries (``/api/v1/query`` with
``time=``) at the end of each of ``--days`` days before ``--enabled-at`` and
each day after it. Instants after ``--now`` are left out.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
import urllib.error
import urllib.request
from collections import defaultdict
from datetime import datetime, timedelta, timezone
from pathlib import Path
from urllib.parse import urlencode

P95_BOUND_S = 30 * 60
MAX_BOUND_S = 4 * 3600
D31_TOLERANCE = 0.20
NOT_RECORDED = "not recorded"
LAND_QUANTILE = 'histogram_quantile({q}, sum by (le) (rate(graft_rpc_duration_seconds_bucket{{job="{job}",op="Land"}}[1d])))'
SCRAPE_UP = 'avg_over_time(up{{job="{job}",service="graft-server"}}[1d])'
FMT = "%Y-%m-%dT%H:%M:%SZ"


def parse_time(text: str) -> datetime:
    return datetime.strptime(text, FMT).replace(tzinfo=timezone.utc)


def load_jsonl(path: Path | None) -> list[dict]:
    if path is None or not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def percentile(values: list[float], pct: float) -> float | None:
    """Nearest-rank percentile: always one of the samples."""
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(1, math.ceil(pct / 100 * len(ordered))) - 1]


def overlap_s(start: datetime, end: datetime, windows: list[tuple[datetime, datetime]]) -> float:
    return sum(max(0.0, (min(end, b) - max(start, a)).total_seconds()) for a, b in windows)


def lands(rows: list[dict]) -> dict[str, dict]:
    """Per changeset: first observation and first settlement, with late flags."""
    out: dict[str, dict] = {}
    for row in sorted((r for r in rows if r["kind"] in ("RefMoved", "LandCheck")), key=lambda r: r["observed_at"]):
        land = out.setdefault(row["changeset"], {"changeset": row["changeset"], "seen": row, "settled": None, "bad": None, "reverted": None, "checked": False})
        if row["kind"] != "LandCheck":
            continue
        land["checked"] = True
        state = row["state"]
        if state in ("good", "bad", "reverted") and land["settled"] is None:
            land["settled"] = row
        if state in ("bad", "reverted") and land["bad"] is None:
            land["bad"] = row
        if state == "reverted" and land["reverted"] is None:
            land["reverted"] = row
    return out


def m1_rows(rows: list[dict], blocked: list[tuple[datetime, datetime]] | None) -> list[dict]:
    """One row per land day, then ``all``. Lands with no land-checks row are
    ``unchecked`` (landed before the line was post-land) and not sampled."""
    by_day: dict[str, list[dict]] = defaultdict(list)
    unchecked: dict[str, int] = defaultdict(int)
    for land in lands(rows).values():
        day = land["seen"]["observed_at"][:10]
        if land["checked"]:
            by_day[day].append(land)
        else:
            unchecked[day] += 1
    table = []
    everything: list[dict] = []
    for day in sorted(set(by_day) | set(unchecked)):
        end = parse_time(day + "T00:00:00Z") + timedelta(days=1)
        table.append(m1_summary(day, by_day[day], blocked, settle_by=end) | {"unchecked": unchecked[day]})
        everything += by_day[day]
    table.append(m1_summary("all", everything, blocked, settle_by=None) | {"unchecked": sum(unchecked.values())})
    return table


def m1_summary(label: str, group: list[dict], blocked: list[tuple[datetime, datetime]] | None, *, settle_by: datetime | None) -> dict:
    values: list[float] = []
    late = pending = 0
    held = 0.0
    for land in group:
        settled = land["settled"]
        if settled is None or (settle_by is not None and parse_time(settled["observed_at"]) >= settle_by):
            pending += 1
            continue
        if land["seen"].get("late") or settled.get("late"):
            late += 1
            continue
        start, end = parse_time(land["seen"]["observed_at"]), parse_time(settled["observed_at"])
        cut = overlap_s(start, end, blocked) if blocked else 0.0
        held += cut
        values.append((end - start).total_seconds() - cut)
    p95 = percentile(values, 95)
    maximum = max(values) if values else None
    return {
        "day": label,
        "lands": len(group),
        "samples": len(values),
        "p50_s": percentile(values, 50),
        "p95_s": p95,
        "max_s": maximum,
        "over_30m": sum(1 for v in values if v > P95_BOUND_S),
        "over_4h": sum(1 for v in values if v > MAX_BOUND_S),
        "within_bound": None if not values else (p95 <= P95_BOUND_S and maximum <= MAX_BOUND_S),
        "late": late,
        "pending": pending,
        "blocked_s": held if blocked is not None else NOT_RECORDED,
    }


def bad_to_reverted(rows: list[dict]) -> dict:
    values = [
        (parse_time(land["reverted"]["observed_at"]) - parse_time(land["bad"]["observed_at"])).total_seconds()
        for land in lands(rows).values()
        if land["bad"] is not None and land["reverted"] is not None and land["bad"]["state"] == "bad"
    ]
    return {"count": len(values), "p50_s": percentile(values, 50), "max_s": max(values) if values else None}


def m2_rows(rows: list[dict], plants: list[dict], batches: list[dict], checker_actor: str | None) -> list[dict]:
    reverts = {r["changeset"]: r for r in rows if r["kind"] == "LandReverted"}
    landed_by = {r["changeset"]: r.get("actor") for r in rows if r["kind"] == "RefMoved"}
    last_state: dict[str, str] = {}
    for row in sorted((r for r in rows if r["kind"] == "LandCheck"), key=lambda r: r["observed_at"]):
        last_state[row["changeset"]] = row["state"]
    table = []
    for plant in plants:
        cs = plant["plant"]
        revert = reverts.get(cs, {}).get("revert")
        actor = landed_by.get(revert) if revert else None
        containing = [b["pr"] for b in batches if cs in b["changesets"]]
        table.append(
            {
                "plant": cs,
                "variant": plant.get("variant"),
                "plant_state": last_state.get(cs),
                "reverted_by": revert,
                "revert_actor": actor,
                "revert_state": last_state.get(revert) if revert else None,
                "unattended": None if checker_actor is None or actor is None else actor == checker_actor,
                "batches": containing,
                "never_alone": all(revert in b["changesets"] for b in batches if cs in b["changesets"]) if revert else not containing,
            }
        )
    return table


def m3_rows(rows: list[dict]) -> list[dict]:
    reviewed = {r["changeset"] for r in rows if r["kind"] == "Attested" and r.get("attest_kind") == "review"}
    by_day: dict[str, list[str]] = defaultdict(list)
    for land in lands(rows).values():
        by_day[land["seen"]["observed_at"][:10]].append(land["changeset"])
    return [
        {"day": day, "lands": len(css), "reviewed": sum(1 for cs in css if cs in reviewed), "unreviewed": sum(1 for cs in css if cs not in reviewed)}
        for day, css in sorted(by_day.items())
    ]


def fetch_json(url: str) -> dict:
    try:
        with urllib.request.urlopen(url, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as err:
        raise RuntimeError(f"prometheus answered {err.code}") from None


def instant(base_url: str, query: str, at: datetime) -> float | None:
    """One instant query at ``at``. None when Prometheus has no value."""
    url = base_url.rstrip("/") + "/api/v1/query?" + urlencode({"query": query, "time": at.strftime(FMT)})
    result = (fetch_json(url).get("data") or {}).get("result") or []
    if not result:
        return None
    value = float(result[0]["value"][1])
    return None if math.isnan(value) else value


def d31_instants(enabled_at: datetime, days: int, now: datetime) -> list[tuple[str, datetime]]:
    """End of each 1d window: ``days`` before enablement, then ``days`` after."""
    before = [("before", enabled_at - timedelta(days=k)) for k in range(days - 1, -1, -1)]
    after = [("after", enabled_at + timedelta(days=k)) for k in range(1, days + 1)]
    return [(side, at) for side, at in before + after if at <= now]


def d31_rows(base_url: str, job: str, enabled_at: datetime, days: int, now: datetime) -> list[dict]:
    p50 = LAND_QUANTILE.format(q="0.5", job=job)
    p95 = LAND_QUANTILE.format(q="0.95", job=job)
    up = SCRAPE_UP.format(job=job)
    return [
        {"side": side, "at": at.strftime(FMT), "p50_s": instant(base_url, p50, at), "p95_s": instant(base_url, p95, at), "up": instant(base_url, up, at)}
        for side, at in d31_instants(enabled_at, days, now)
    ]


def d31_verdict(table: list[dict]) -> dict:
    before = [r["p95_s"] for r in table if r["side"] == "before" and r["p95_s"] is not None]
    after = [r["p95_s"] for r in table if r["side"] == "after" and r["p95_s"] is not None]
    ceiling = max(before) * (1 + D31_TOLERANCE) if before else None
    holds = None if ceiling is None or not after else all(v <= ceiling for v in after)
    return {"before_days": len(before), "after_days": len(after), "ceiling_s": ceiling, "holds": holds}


def _cell(value: object) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.3f}".rstrip("0").rstrip(".")
    return str(value)


def _table(rows: list[dict], keys: list[str]) -> list[str]:
    lines = ["| " + " | ".join(keys) + " |", "|" + " --- |" * len(keys)]
    lines += ["| " + " | ".join(_cell(row[k]) for k in keys) + " |" for row in rows]
    return lines


def render(rows: list[dict], *, interval_s: int, plants: list[dict], batches: list[dict], blocked: list[tuple[datetime, datetime]] | None, checker_actor: str | None, d31: list[dict] | None, job: str) -> str:
    gaps = [r for r in rows if r["kind"] == "Gap"]
    out = [
        "# v0.2 exit tables",
        "",
        "## M1 land-to-settled",
        "",
        f"Bound: p95 <= {P95_BOUND_S}s and max <= {MAX_BOUND_S}s, Blocked time excluded. "
        f"Times are recorder observations (the server journals none), so each sample is exact to one poll interval ({interval_s}s). "
        "Land time is the first observation of the land; settle time is the first poll that read it good, bad or reverted.",
        "",
    ]
    out += _table(m1_rows(rows, blocked), ["day", "lands", "unchecked", "samples", "p50_s", "p95_s", "max_s", "over_30m", "over_4h", "within_bound", "late", "pending", "blocked_s"])
    if blocked is None:
        out += ["", "Blocked time: not recorded. The server keeps no blocked state; pass `--blocked` with the operator's windows."]
    btr = bad_to_reverted(rows)
    out += ["", f"bad-to-reverted: {btr['count']} lands, p50 {_cell(btr['p50_s'])}s, max {_cell(btr['max_s'])}s.", ""]
    out += ["### Recorder gaps", ""]
    out += _table(gaps, ["from", "to", "seconds"]) if gaps else ["None."]
    out += ["", "## M2 plants", ""]
    out += _table(m2_rows(rows, plants, batches, checker_actor), ["plant", "variant", "plant_state", "reverted_by", "revert_actor", "revert_state", "unattended", "batches", "never_alone"])
    out += ["", "## M3 reviews", "", "The review-queue check and the session's tool log are read separately.", ""]
    out += _table(m3_rows(rows), ["day", "lands", "reviewed", "unreviewed"])
    out += ["", "## D31 Land latency", ""]
    if d31 is None:
        out += ["Not run: pass `--prometheus` and `--enabled-at`."]
    else:
        verdict = d31_verdict(d31)
        out += [
            "Instant queries (`/api/v1/query`, `time=` the row's `at`):",
            "",
            f"- p50: `{LAND_QUANTILE.format(q='0.5', job=job)}`",
            f"- p95: `{LAND_QUANTILE.format(q='0.95', job=job)}`",
            f"- up: `{SCRAPE_UP.format(job=job)}`",
            "",
        ]
        out += _table(d31, ["side", "at", "p50_s", "p95_s", "up"])
        out += [
            "",
            f"Holds when every after-p95 is at most the highest before-p95 plus {int(D31_TOLERANCE * 100)}% "
            f"(ceiling {_cell(verdict['ceiling_s'])}s, {verdict['before_days']} before and {verdict['after_days']} after days): {_cell(verdict['holds'])}.",
        ]
    out.append("")
    return "\n".join(out)


def load_windows(path: Path | None) -> list[tuple[datetime, datetime]] | None:
    if path is None:
        return None
    return [(parse_time(w["from"]), parse_time(w["to"])) for w in json.loads(path.read_text(encoding="utf-8"))]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--jsonl", type=Path, required=True, help="The recorder's log.")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--interval", type=int, default=60, help="The recorder's poll interval.")
    parser.add_argument("--plants", type=Path, default=None, help="JSONL written by plant.py --record.")
    parser.add_argument("--batches", type=Path, default=None, help='JSON list of {"pr": n, "changesets": [...]} from the mirror.')
    parser.add_argument("--blocked", type=Path, default=None, help='JSON list of {"from": t, "to": t} operator-held windows.')
    parser.add_argument("--checker-actor", default=None, help="The checker's principal; a revert landed by it is unattended.")
    parser.add_argument("--prometheus", default=None, help="Prometheus base URL.")
    parser.add_argument("--job", default="graft-dev")
    parser.add_argument("--enabled-at", default=None, help="Enablement instant, YYYY-MM-DDTHH:MM:SSZ.")
    parser.add_argument("--days", type=int, default=14)
    parser.add_argument("--now", default=None)
    args = parser.parse_args(argv)
    d31 = None
    if args.prometheus:
        if not args.enabled_at:
            parser.error("--prometheus needs --enabled-at")
        now = parse_time(args.now) if args.now else datetime.now(timezone.utc)
        d31 = d31_rows(args.prometheus, args.job, parse_time(args.enabled_at), args.days, now)
    text = render(
        load_jsonl(args.jsonl),
        interval_s=args.interval,
        plants=load_jsonl(args.plants),
        batches=json.loads(args.batches.read_text(encoding="utf-8")) if args.batches else [],
        blocked=load_windows(args.blocked),
        checker_actor=args.checker_actor,
        d31=d31,
        job=args.job,
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text, encoding="utf-8")
    print(args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
