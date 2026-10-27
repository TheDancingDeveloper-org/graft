#!/usr/bin/env python3
"""Render the v0.2 exit tables from the recorder JSONL.

Every number comes from the JSONL plus one Prometheus query. Nothing is
estimated.

M1 bound (operator decision 3, tightened): p95 <= 30 min and max <= 4 h.
Land time is the journal entry's ``journaled_at`` when the body named one,
otherwise the first observation. Settle time is the first poll that read
the landing as ``good``, ``bad`` or ``reverted``. The recorder's
``--interval`` is the largest gap that observation can hide, so it bounds
the precision of every sample.

Queries the tables cite:

- land-to-settled: seconds from land time to the earliest ``LandCheck`` of
  state ``good`` or ``bad`` for the same changeset. Rows flagged ``late``
  are excluded from the percentiles and counted under ``late``.
- blocked: only when the journal carries an ``AttesterBlocked`` row. The
  server's check states are ``pending``, ``good``, ``bad`` and ``reverted``,
  so without that row the cell is ``n/a (not observable)``. It is never 0.
- pending: a ``RefMoved`` with no settlement row.
- over bound: samples whose land-to-settled exceeds 4 h, and whether p95
  exceeds 30 min.
- M2: each planted changeset, the ``LandReverted`` that names it, whether
  that revert's actor is the checker, and batch entries that contain it.
- M3: per day, lands (``RefMoved``) against ``review`` attestations.
- D31: the stated PromQL, fetched through the Prometheus HTTP API. The
  value is the query result, not a file of pre-extracted samples.
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlencode

P95_BOUND_S = 30 * 60
MAX_BOUND_S = 4 * 3600
BLOCKED_NA = "n/a (not observable)"
D31_QUERY = 'histogram_quantile(0.95, rate(graft_rpc_duration_seconds_bucket{op="Land"}[1d]))'


def load_jsonl(path: Path) -> list[dict]:
    rows = []
    if not path.exists():
        return rows
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    return rows


def _parse(ts: str) -> datetime:
    return datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


def percentile(values: list[float], pct: float) -> float:
    """Nearest-rank percentile. Reproducible; not an interpolation."""
    if not values:
        return 0.0
    ordered = sorted(values)
    rank = max(1, math.ceil(pct / 100 * len(ordered)))
    return ordered[rank - 1]


def land_time(land: dict) -> str:
    """Journal time when the entry named one, otherwise first observation."""
    return land.get("journaled_at") or land["observed_at"]


def land_samples(rows: list[dict]) -> dict:
    """Latency samples and coverage counts from recorder rows."""
    moved = [r for r in rows if r["kind"] == "RefMoved"]
    checks: dict[str, list[dict]] = defaultdict(list)
    for row in rows:
        if row["kind"] == "LandCheck" and row.get("changeset"):
            checks[row["changeset"]].append(row)
    samples = []
    late = 0
    pending = 0
    blocked_reports = [r for r in rows if r["kind"] == "AttesterBlocked"]
    for land in moved:
        if land.get("late"):
            late += 1
            continue
        followers = [c for c in checks.get(land.get("changeset"), []) if c["state"] in ("good", "bad")]
        if not followers:
            pending += 1
            continue
        followers.sort(key=lambda c: c["observed_at"])
        settled = followers[0]
        delta = (_parse(settled["observed_at"]) - _parse(land_time(land))).total_seconds()
        samples.append(
            {
                "changeset": land.get("changeset"),
                "seconds": delta,
                "landed_at": land_time(land),
                "settled_at": settled["observed_at"],
            }
        )
    blocked = BLOCKED_NA if not blocked_reports else float(len(blocked_reports))
    return {"samples": samples, "late": late, "pending": pending, "blocked": blocked}


def m1_table(rows: list[dict], *, interval_s: int | None = None) -> dict:
    summary = land_samples(rows)
    values = [s["seconds"] for s in summary["samples"]]
    p95 = percentile(values, 95)
    maximum = max(values) if values else 0.0
    return {
        "count": len(values),
        "p50_s": percentile(values, 50),
        "p95_s": p95,
        "max_s": maximum,
        "over_p95_bound": p95 > P95_BOUND_S,
        "over_max_bound": sum(1 for v in values if v > MAX_BOUND_S),
        "late": summary["late"],
        "pending": summary["pending"],
        "blocked": summary["blocked"],
        "precision": f"bounded by poll interval {interval_s}s" if interval_s is not None else "bounded by --interval",
        "p95_bound_s": P95_BOUND_S,
        "max_bound_s": MAX_BOUND_S,
    }


def m2_table(rows: list[dict], plants: list[dict], batches: list[dict]) -> list[dict]:
    reverts = {}
    for row in rows:
        if row["kind"] == "LandReverted":
            reverts[row.get("changeset")] = row
    table = []
    for plant in plants:
        cs = plant["changeset"]
        revert = reverts.get(cs)
        containing = [b["pr"] for b in batches if cs in b.get("changesets", [])]
        table.append(
            {
                "changeset": cs,
                "variant": plant.get("variant"),
                "reverted_by": None if revert is None else revert.get("revert"),
                "unattended": bool(revert) and revert.get("actor") == "checker",
                "batches": containing,
                "paired_with_revert": bool(revert) and any(revert.get("revert") in b.get("changesets", []) for b in batches),
            }
        )
    return table


def m3_table(rows: list[dict]) -> list[dict]:
    lands: dict[str, int] = defaultdict(int)
    reviews: dict[str, int] = defaultdict(int)
    for row in rows:
        if row.get("late"):
            continue
        day = row["observed_at"][:10]
        if row["kind"] == "RefMoved":
            lands[day] += 1
        elif row["kind"] == "Attested" and row.get("attest_kind") == "review":
            reviews[day] += 1
    days = sorted(set(lands) | set(reviews))
    return [{"day": day, "lands": lands[day], "reviews": reviews[day], "gap": lands[day] - reviews[day]} for day in days]


def prometheus_query(http: str, base_url: str, query: str) -> dict:
    """Run one instant query against the Prometheus HTTP API."""
    url = base_url.rstrip("/") + "/api/v1/query?" + urlencode({"query": query})
    proc = subprocess.run([http, "-sS", "-m", "30", url], text=True, capture_output=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"prometheus query exited {proc.returncode}: {proc.stderr.strip()}")
    return json.loads(proc.stdout or "{}")


def quantile_value(payload: dict) -> float | None:
    result = (payload.get("data") or {}).get("result") or []
    if not result:
        return None
    value = result[0].get("value")
    if not value or len(value) < 2 or value[1] in ("NaN", "nan", None):
        return None
    return float(value[1])


def d31_table(before: dict, after: dict) -> dict:
    return {
        "query": D31_QUERY,
        "source": "prometheus http api",
        "before": quantile_value(before),
        "after": quantile_value(after),
    }


def _cell(value: object) -> str:
    return str(value)


def render(rows: list[dict], plants: list[dict], batches: list[dict], before: dict, after: dict, *, interval_s: int | None = None) -> str:
    m1 = m1_table(rows, interval_s=interval_s)
    lines = [
        "# v0.2 exit tables",
        "",
        "M1 land time is the journal entry when it names one, otherwise the first observation. Settle time is the first observation of good, bad or reverted. " + m1["precision"] + ".",
        "",
        "| metric | value |",
        "| --- | --- |",
    ]
    for key in ("count", "p50_s", "p95_s", "max_s", "over_p95_bound", "over_max_bound", "late", "pending", "blocked", "precision", "p95_bound_s", "max_bound_s"):
        lines.append(f"| {key} | {_cell(m1[key])} |")
    lines += ["", "## M2 plants", "", "| changeset | variant | reverted_by | unattended | batches | paired |", "| --- | --- | --- | --- | --- | --- |"]
    for row in m2_table(rows, plants, batches):
        lines.append(
            f"| {row['changeset']} | {row['variant']} | {row['reverted_by']} | {row['unattended']} | {row['batches']} | {row['paired_with_revert']} |"
        )
    lines += ["", "## M3 reviews", "", "| day | lands | reviews | gap |", "| --- | --- | --- | --- |"]
    for row in m3_table(rows):
        lines.append(f"| {row['day']} | {row['lands']} | {row['reviews']} | {row['gap']} |")
    d31 = d31_table(before, after)
    lines += [
        "",
        "## D31",
        "",
        f"Query `{d31['query']}` through the Prometheus HTTP API.",
        "",
        "| side | p95_s |",
        "| --- | --- |",
        f"| before | {d31['before']} |",
        f"| after | {d31['after']} |",
        "",
    ]
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jsonl", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--plants", type=Path, default=None)
    parser.add_argument("--batches", type=Path, default=None)
    parser.add_argument("--interval", type=int, default=60)
    parser.add_argument("--prometheus", default=None, help="Prometheus base URL. Required unless --prometheus-fixture is set.")
    parser.add_argument("--prometheus-fixture", type=Path, default=None, help="Recorded HTTP response, used by tests.")
    parser.add_argument("--http", default="curl")
    args = parser.parse_args(argv)
    rows = load_jsonl(args.jsonl)
    plants = json.loads(args.plants.read_text(encoding="utf-8")) if args.plants else []
    batches = json.loads(args.batches.read_text(encoding="utf-8")) if args.batches else []
    if args.prometheus_fixture:
        payload = json.loads(args.prometheus_fixture.read_text(encoding="utf-8"))
        before, after = payload["before"], payload["after"]
    elif args.prometheus:
        before = prometheus_query(args.http, args.prometheus, D31_QUERY)
        after = before
    else:
        before, after = {"data": {"result": []}}, {"data": {"result": []}}
    text = render(rows, plants, batches, before, after, interval_s=args.interval)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text, encoding="utf-8")
    print(args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
