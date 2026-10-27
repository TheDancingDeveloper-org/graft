#!/usr/bin/env python3
"""Render the v0.2 exit tables from the recorder JSONL.

Every number comes from the JSONL plus the stated queries. Nothing is
estimated.

M1 bound (operator decision 3, tightened): p95 <= 30 min and max <= 4 h.
Time a line is held Blocked is reported on its own row and is not part of
land-to-settled.

Queries the tables cite:

- land-to-settled: for each ``RefMoved``, seconds from its ``observed_at``
  to the earliest later ``LandCheck`` of state ``good`` or ``bad`` for the
  same ``pos``. Rows flagged ``late`` are excluded from the percentiles and
  counted under ``late``.
- blocked: seconds a landing's check state reads ``blocked`` between its
  ``RefMoved`` and its settlement, summed. Recorded as ``LandCheck`` rows
  whose ``state`` is ``blocked``.
- pending: a ``RefMoved`` with no settlement row.
- over bound: samples whose land-to-settled exceeds 4 h, and whether p95
  exceeds 30 min.
- M2: each planted changeset, the ``LandReverted`` that names it, whether
  that revert's actor is the checker, and batch entries that contain it.
- M3: per day, lands (``RefMoved``) against ``review`` attestations.
- D31: Prometheus ``histogram_quantile`` of
  ``graft_rpc_duration_seconds{op="Land"}`` before and after the window.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

P95_BOUND_S = 30 * 60
MAX_BOUND_S = 4 * 3600


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


def land_samples(rows: list[dict]) -> dict:
    """Latency samples and coverage counts from recorder rows."""
    moved = [r for r in rows if r["kind"] == "RefMoved"]
    checks: dict[int, list[dict]] = defaultdict(list)
    for row in rows:
        if row["kind"] == "LandCheck" and row.get("pos") is not None:
            checks[int(row["pos"])].append(row)
    samples = []
    late = 0
    pending = 0
    blocked_s = 0.0
    for land in moved:
        if land.get("late"):
            late += 1
            continue
        pos = land.get("pos")
        followers = [c for c in checks.get(int(pos), []) if c["state"] in ("good", "bad")]
        if not followers:
            pending += 1
            continue
        followers.sort(key=lambda c: c["observed_at"])
        settled = followers[0]
        delta = (_parse(settled["observed_at"]) - _parse(land["observed_at"])).total_seconds()
        blocked = [c for c in checks.get(int(pos), []) if c["state"] == "blocked"]
        held = 0.0
        if blocked:
            blocked.sort(key=lambda c: c["observed_at"])
            released = next(
                (c for c in checks.get(int(pos), []) if c["state"] in ("good", "bad", "unblocked") and c["observed_at"] >= blocked[0]["observed_at"]),
                None,
            )
            if released:
                held = (_parse(released["observed_at"]) - _parse(blocked[0]["observed_at"])).total_seconds()
        blocked_s += held
        samples.append({"pos": pos, "changeset": land.get("changeset"), "seconds": delta - held, "blocked_s": held})
    return {"samples": samples, "late": late, "pending": pending, "blocked_s": blocked_s}


def m1_table(rows: list[dict]) -> dict:
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
        "blocked_s": summary["blocked_s"],
        "p95_bound_s": P95_BOUND_S,
        "max_bound_s": MAX_BOUND_S,
    }


def m2_table(rows: list[dict], plants: list[dict], batches: list[dict]) -> list[dict]:
    """One row per planted changeset.

    ``plants`` is ``[{changeset, variant}]``. ``batches`` is
    ``[{pr, changesets: [...]}]`` from the batch trailers.
    """
    reverts = {}
    for row in rows:
        if row["kind"] == "LandReverted":
            reverts[row.get("changeset")] = row
    table = []
    for plant in plants:
        cs = plant["changeset"]
        revert = reverts.get(cs)
        containing = [b["pr"] for b in batches if cs in b.get("changesets", [])]
        paired = False
        if revert and revert.get("revert"):
            paired = all(revert["revert"] in b.get("changesets", []) for b in batches if cs in b.get("changesets", [])) or not containing
        table.append(
            {
                "changeset": cs,
                "variant": plant.get("variant"),
                "reverted_by": None if revert is None else revert.get("revert"),
                "actor": None if revert is None else revert.get("actor"),
                "unattended": bool(revert and revert.get("actor") == "checker"),
                "batches": containing,
                "paired_with_revert": bool(revert) and paired,
            }
        )
    return table


def m3_table(rows: list[dict]) -> list[dict]:
    """Per day: lands against review attestations."""
    days: dict[str, dict] = defaultdict(lambda: {"lands": 0, "reviews": 0})
    for row in rows:
        day = row["observed_at"][:10]
        if row["kind"] == "RefMoved" and not row.get("late"):
            days[day]["lands"] += 1
        if row["kind"] == "Attested" and row.get("attest_kind", "review") == "review":
            days[day]["reviews"] += 1
    return [{"day": day, **days[day], "gap": days[day]["lands"] - days[day]["reviews"]} for day in sorted(days)]


def d31_table(before: list[float], after: list[float]) -> dict:
    """Before/after from Prometheus ``graft_rpc_duration_seconds{op="Land"}``.

    ``before`` and ``after`` are the raw observation seconds the query
    ``histogram_quantile(0.95, rate(...[1d]))`` was computed from, one sample
    per series point the caller extracted. The table reports p50, p95 and max
    of each side so the page can be recomputed from those samples.
    """
    def side(values: list[float]) -> dict:
        return {"p50_s": percentile(values, 50), "p95_s": percentile(values, 95), "max_s": max(values) if values else 0.0, "n": len(values)}

    return {"query": 'histogram_quantile(0.95, rate(graft_rpc_duration_seconds_bucket{op="Land"}[1d]))', "before": side(before), "after": side(after)}


def render(rows: list[dict], plants: list[dict], batches: list[dict], before: list[float], after: list[float]) -> str:
    m1 = m1_table(rows)
    lines = ["# v0.2 exit report", "", "## M1 land-to-settled", "",
             "| metric | value |", "| --- | --- |"]
    for key in ("count", "p50_s", "p95_s", "max_s", "over_p95_bound", "over_max_bound", "late", "pending", "blocked_s"):
        lines.append(f"| {key} | {m1[key]} |")
    lines += ["", f"Bound: p95 <= {P95_BOUND_S}s, max <= {MAX_BOUND_S}s. Blocked time is excluded from the samples.", ""]
    lines += ["## M2 plants", "", "| changeset | variant | reverted by | unattended | batches | paired |", "| --- | --- | --- | --- | --- | --- |"]
    for row in m2_table(rows, plants, batches):
        lines.append(
            f"| {row['changeset']} | {row['variant']} | {row['reverted_by']} | {row['unattended']} | {','.join(map(str, row['batches'])) or '-'} | {row['paired_with_revert']} |"
        )
    lines += ["", "## M3 review days", "", "| day | lands | reviews | gap |", "| --- | --- | --- | --- |"]
    for row in m3_table(rows):
        lines.append(f"| {row['day']} | {row['lands']} | {row['reviews']} | {row['gap']} |")
    d31 = d31_table(before, after)
    lines += ["", "## D31 Land RPC", "", f"Query: `{d31['query']}`", "", "| side | n | p50 | p95 | max |", "| --- | --- | --- | --- | --- |"]
    for side in ("before", "after"):
        s = d31[side]
        lines.append(f"| {side} | {s['n']} | {s['p50_s']} | {s['p95_s']} | {s['max_s']} |")
    lines.append("")
    return "\n".join(lines)


def _floats(path: Path | None) -> list[float]:
    if path is None or not path.exists():
        return []
    return [float(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jsonl", type=Path, required=True)
    parser.add_argument("--plants", type=Path, default=None, help="JSON list of planted changesets")
    parser.add_argument("--batches", type=Path, default=None, help="JSON list of batch PRs")
    parser.add_argument("--before", type=Path, default=None, help="Land RPC samples before, one float per line")
    parser.add_argument("--after", type=Path, default=None)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    plants = json.loads(args.plants.read_text(encoding="utf-8")) if args.plants else []
    batches = json.loads(args.batches.read_text(encoding="utf-8")) if args.batches else []
    text = render(load_jsonl(args.jsonl), plants, batches, _floats(args.before), _floats(args.after))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text, encoding="utf-8")
    print(args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
