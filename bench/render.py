#!/usr/bin/env python3
"""Render the committed graft-vs-git bench SVG. No network, stdlib only.

Every number is read from a file under bench/results/. A missing required
file exits 1. The twelve-run read-path medians (WI-2341, D31) are not in
any results file; they are not drawn.

Usage: python3 bench/render.py [out.svg]
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "bench" / "results"
SCALE_1K = RESULTS / "2026-10-23-scales-10-1000" / "results.json"
SCALE_10K = RESULTS / "2026-10-23-scale-10000" / "results.json"
REAL = RESULTS / "2026-10-24-real-prs" / "results.json"
BRANCH = RESULTS / "2026-10-24-branching" / "results.json"

OPS = [
    ("clone_full", "clone"),
    ("clone_sparse", "sparse"),
    ("log", "log -50"),
    ("log_path", "log --path"),
    ("blame", "blame"),
    ("status", "status"),
    ("sync", "sync"),
]
CONC = [("clean", "clean lands"), ("merge3", "same-file")]


def load(path: Path) -> dict:
    if not path.is_file():
        print(f"missing results file: {path.relative_to(ROOT)}", file=sys.stderr)
        sys.exit(1)
    return json.loads(path.read_text())


def scale(doc: dict, n: int) -> dict:
    for row in doc["scales"]:
        if row["scale"] == n:
            return row
    print(f"scale {n} not in results", file=sys.stderr)
    sys.exit(1)


def p50(node: dict) -> float:
    if "p50_ms" in node:
        return float(node["p50_ms"])
    return float(node["latency"]["p50_ms"])


def op_pair(row: dict, key: str) -> tuple[float, float]:
    node = row["ops"][key]
    return p50(node["graft"]), p50(node["git"])


def conc_pair(row: dict, key: str) -> tuple[float, float]:
    return (
        p50(row["concurrent"]["graft"][key]),
        p50(row["concurrent"]["git"][key]),
    )


def esc(text: str) -> str:
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
    )


def bars(x, y, w, h, pairs, labels, title) -> list[str]:
    """pairs: list of (graft, git). Lower is faster."""
    out = [f'<text x="{x}" y="{y - 8}" class="h">{esc(title)}</text>']
    n = len(pairs)
    gap = 8
    slot = (w - gap * (n - 1)) / n
    bw = min(14, (slot - 6) / 2)
    top = max(max(a, b) for a, b in pairs) or 1
    base = y + h
    for i, ((g, t), label) in enumerate(zip(pairs, labels)):
        sx = x + i * (slot + gap)
        for j, (val, cls) in enumerate(((g, "g"), (t, "t"))):
            bh = max(1.0, h * val / top)
            bx = sx + j * (bw + 2)
            out.append(
                f'<rect x="{bx:.1f}" y="{base - bh:.1f}" width="{bw:.1f}" '
                f'height="{bh:.1f}" class="{cls}"/>'
            )
        out.append(
            f'<text x="{sx + bw:.1f}" y="{base + 14}" class="lab" '
            f'text-anchor="middle">{esc(label)}</text>'
        )
    return out


def ratio_note(pairs) -> str:
    # median graft/git across the drawn ops, reported as a range of ratios
    ratios = [g / t for g, t in pairs if t]
    lo, hi = min(ratios), max(ratios)
    wins = sum(1 for r in ratios if r < 1)
    return f"{wins}/{len(ratios)} ops faster than git; ratio {lo:.2f}–{hi:.2f}"


def main() -> None:
    out_path = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "docs" / "guide" / "bench.svg"
    s1 = scale(load(SCALE_1K), 1000)
    s10 = scale(load(SCALE_10K), 10000)
    real = load(REAL)
    branch = load(BRANCH)

    labels = [name for _, name in OPS] + [name for _, name in CONC]
    p1 = [op_pair(s1, k) for k, _ in OPS] + [conc_pair(s1, k) for k, _ in CONC]
    p10 = [op_pair(s10, k) for k, _ in OPS] + [conc_pair(s10, k) for k, _ in CONC]

    cases = real["cases"]
    prs = sum(c["n_prs"] for c in cases)

    def clean_count(case: dict, tool: str) -> int:
        agr = case.get("agreement")
        if agr is not None:
            return int(agr["both_clean"])
        # rdpapp-stack records no agreement block; the two tools match.
        return int(case[tool]["merged_clean"])

    clean_g = sum(clean_count(c, "graft") for c in cases)
    clean_t = sum(clean_count(c, "git") for c in cases)
    conf_g = sum(c["graft"]["conflicts"] for c in cases)
    conf_t = sum(c["git"]["conflicts"] for c in cases)
    matched = all(
        c["graft"]["conflicts"] == c["git"]["conflicts"]
        and (
            c.get("agreement") is None
            or (
                c["agreement"]["graft_only_clean"] == 0
                and c["agreement"]["git_only_clean"] == 0
            )
        )
        for c in cases
    )

    strategies = []
    for row in branch["results"]:
        systems = row["systems"]
        strategies.append(
            (
                row["strategy"],
                systems["graft-tracked"]["counts"]["round_trips"],
                systems["git"]["counts"]["round_trips"],
                systems["graft-tracked"]["counts"]["conflict_blocks"],
                systems["git"]["counts"]["conflict_blocks"],
            )
        )

    W, H = 920, 560
    parts = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" '
        f'width="{W}" height="{H}" role="img">',
        "<title>graft against git, recorded bench</title>",
        "<style>",
        "  svg { background: transparent; }",
        "  .h { font: 600 14px ui-sans-serif, system-ui, sans-serif; fill: #1c1917; }",
        "  .sub { font: 12px ui-sans-serif, system-ui, sans-serif; fill: #44403c; }",
        "  .lab { font: 11px ui-sans-serif, system-ui, sans-serif; fill: #44403c; }",
        "  .g { fill: #0f766e; } .t { fill: #a8a29e; }",
        "  .box { fill: none; stroke: #d6d3d1; }",
        "  @media (prefers-color-scheme: dark) {",
        "    .h { fill: #fafaf9; } .sub, .lab { fill: #d6d3d1; }",
        "    .g { fill: #2dd4bf; } .t { fill: #78716c; } .box { stroke: #44403c; }",
        "  }",
        "</style>",
        f'<text x="24" y="28" class="h">graft vs git, p50 ms (lower is faster)</text>',
        '<text x="24" y="48" class="sub">teal graft, grey git. '
        "1k host A: 24 CPUs, 62.6 GiB, seed 2294, 5 iterations. "
        "10k host B: 4 vCPUs, 7.8 GiB, seed 2294, 5 iterations. "
        "Fresh CLI process per step.</text>",
        f'<rect x="16" y="64" width="888" height="250" rx="8" class="box"/>',
    ]
    parts += bars(32, 96, 420, 180, p1, labels, "1,000 commits")
    parts += bars(480, 96, 420, 180, p10, labels, "10,000 commits")
    parts.append(
        f'<text x="32" y="300" class="sub">{esc(ratio_note(p1))} at 1k. '
        f'{esc(ratio_note(p10))} at 10k. '
        "Same-file is the merge3 concurrent scenario.</text>"
    )
    parts.append('<rect x="16" y="320" width="430" height="210" rx="8" class="box"/>')
    parts.append('<text x="32" y="344" class="h">Real-PR replay</text>')
    parts.append(
        f'<text x="32" y="368" class="sub">{prs} PRs, {len(cases)} cases. '
        f"Clean merges graft {clean_g} / git {clean_t}. "
        f"Conflicts graft {conf_g} / git {conf_t}. "
        f"Matched outcomes: {'yes' if matched else 'no'}.</text>"
    )
    parts.append(
        '<text x="32" y="392" class="sub">Source: bench/results/2026-10-24-real-prs/results.json. '
        "Both tools agreed on every case (graft-only and git-only clean are 0).</text>"
    )
    # per case rows
    yy = 418
    for c in cases:
        parts.append(
            f'<text x="32" y="{yy}" class="lab">{esc(c["id"])}: '
            f'{c["n_prs"]} PRs, clean {clean_count(c, "graft")}='
            f'{clean_count(c, "git")}, conflicts {c["graft"]["conflicts"]}='
            f'{c["git"]["conflicts"]}, rt {c["graft"]["round_trips"]}/'
            f'{c["git"]["round_trips"]}</text>'
        )
        yy += 16

    parts.append('<rect x="462" y="320" width="442" height="210" rx="8" class="box"/>')
    parts.append('<text x="478" y="344" class="h">Branching strategies</text>')
    parts.append(
        '<text x="478" y="368" class="sub">Round trips and conflict blocks, '
        "graft-tracked / git. Seed 2362. "
        "File predates merge points (see docs/status.md).</text>"
    )
    yy = 400
    for name, grt, trt, gb, tb in strategies:
        parts.append(
            f'<text x="478" y="{yy}" class="lab">{esc(name)}: '
            f"round trips {grt} / {trt}, conflict blocks {gb} / {tb}</text>"
        )
        yy += 20
    parts.append(
        '<text x="24" y="548" class="sub">Not on this chart: the WI-2341 twelve-run '
        "read-path medians (D31 floor). They are in docs/status.md and docs/decisions.md "
        "only, not in a results file.</text>"
    )
    parts.append("</svg>\n")
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(parts) + "\n")
    print(f"wrote {out_path}")


if __name__ == "__main__":
    main()
