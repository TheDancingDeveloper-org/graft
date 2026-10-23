"""graft-bench: python3 -m graftbench --help (run from bench/)."""

from __future__ import annotations

import argparse
import datetime
import json
import shutil
import sys
import tempfile
import time
import traceback
from pathlib import Path

from . import bench, livea, report
from .proc import StepError
from .site import LocalSite, RemoteSite
from .tools import Git, Graft, Mirror

ROOT = Path(__file__).resolve().parents[2]
SCALES = [10, 1000, 10000, 100000]


def parse_args(argv):
    p = argparse.ArgumentParser(prog="graftbench", description=__doc__)
    p.add_argument(
        "--scales", default="10,1000", help="comma-separated commit counts (default 10,1000; the full set is 10,1000,10000,100000)"
    )
    p.add_argument("--seed", type=int, default=2294)
    p.add_argument("--writers", type=int, default=8, help="concurrent writers (default 8)")
    p.add_argument("--rounds", type=int, default=5, help="rounds per concurrent scenario (default 5)")
    p.add_argument("--iterations", type=int, default=5, help="samples per single-agent operation (default 5)")
    p.add_argument("--out", type=Path, help="results directory (default results/<UTC time>)")
    p.add_argument("--label", help="a name for the run in the summary")
    p.add_argument("--work", type=Path, help="scratch directory (default: a temp dir, removed unless --keep)")
    p.add_argument("--keep", action="store_true", help="keep the scratch directory")
    bins = ROOT / "target" / "release"
    p.add_argument("--graft-bin", type=Path, default=bins / "graft")
    p.add_argument("--server-bin", type=Path, default=bins / "graft-server")
    p.add_argument("--mirror-bin", type=Path, default=bins / "graft-mirror")
    p.add_argument("--git-bin", default="git")
    p.add_argument("--server-url", help="use this running server instead of starting one")
    p.add_argument(
        "--token-file", type=Path, help="with --server-url: the bearer token file (read, write, land:main, admin on the repositories)"
    )
    p.add_argument("--repo-prefix", default="bench", help="repositories are <prefix>-<scale> and <prefix>-<scale>-import")
    p.add_argument("--no-import", action="store_true", help="skip the graft-mirror import benchmark (and its repository)")
    p.add_argument("--live-awareness", action="store_true", help="also run the deterministic live-awareness scenario (WI-2311)")
    p.add_argument("--overlap", default="mid", help="live awareness: comma-separated overlap levels from low,mid,high (default mid)")
    p.add_argument("--la-writers", type=int, default=4, help="live awareness: writers (default 4)")
    p.add_argument("--la-steps", type=int, default=6, help="live awareness: steps per writer (default 6)")
    p.add_argument("--racing", action="store_true", help="live awareness: also run each graft policy with real threads (correctness only)")
    a = p.parse_args(argv)
    a.scales = [int(x) for x in a.scales.split(",") if x]
    a.overlap = [x for x in a.overlap.split(",") if x]
    for o in a.overlap:
        if o not in livea.OVERLAPS:
            p.error(f"--overlap: {o!r} is not one of low, mid, high")
    if bool(a.server_url) != bool(a.token_file):
        p.error("--server-url and --token-file go together")
    for b in (a.graft_bin, a.mirror_bin) + (() if a.server_url else (a.server_bin,)):
        if not b.is_file():
            p.error(f"{b} not found: build with `cargo build --release -p graft-cli -p graft-server -p graft-mirror`, or pass its path")
    return a


def main(argv=None) -> int:
    a = parse_args(argv if argv is not None else sys.argv[1:])
    started = datetime.datetime.now(datetime.timezone.utc)
    out = a.out or Path("results") / started.strftime("%Y%m%dT%H%M%SZ")
    out.mkdir(parents=True, exist_ok=True)
    work = a.work or Path(tempfile.mkdtemp(prefix="graft-bench-"))
    work.mkdir(parents=True, exist_ok=True)

    def log(msg: str) -> None:
        print(f"{time.strftime('%H:%M:%S')} {msg}", flush=True)

    repos = {s: (f"{a.repo_prefix}-{s}", None if a.no_import else f"{a.repo_prefix}-{s}-import") for s in a.scales}
    git = Git(a.git_bin)
    if a.server_url:
        site = RemoteSite(a.server_url, a.token_file)
    else:
        names = [n for pair in repos.values() for n in pair if n]
        if a.live_awareness:
            names += livea.repo_names(a.repo_prefix, a.overlap, a.racing)
        site = LocalSite(a.server_bin, work / "server", names)
    log(f"server {site.kind} at {site.url}; scratch {work}")
    graft = Graft(a.graft_bin, site.url, site.token_file)
    mirror = Mirror(a.mirror_bin, site.url, site.token_file)
    doc = {
        "label": a.label or out.name,
        "started": started.isoformat(timespec="seconds"),
        "seed": a.seed,
        "writers": a.writers,
        "rounds": a.rounds,
        "iterations": a.iterations,
        "server": site.kind,
        "machine": report.machine(git.version(), graft.version(), ROOT),
        "scales": [],
    }
    cfg = bench.Config(a.seed, a.writers, a.rounds, a.iterations, work, log)
    t0 = time.perf_counter()
    rc = 0
    try:
        for scale in a.scales:
            repo, imp = repos[scale]
            try:
                r = bench.run_scale(cfg, scale, site, graft, mirror, git, repo, imp)
            except StepError as e:
                log(f"[{scale}] failed: {e}")
                doc["scales"].append({"scale": scale, "error": str(e)})
                rc = 2
                break
            doc["scales"].append(r)
            if not r["ok"]:
                rc = 1
            log(f"[{scale}] {'pass' if r['ok'] else 'FAIL'}")
            _write(out, doc, time.perf_counter() - t0)
        if a.live_awareness and rc != 2:
            ctx = livea.Ctx(graft, mirror, git, work, log)
            try:
                la = livea.run_mode(ctx, site, a.repo_prefix, a.seed, a.la_writers, a.la_steps, a.overlap, a.racing)
            except StepError as e:
                log(f"[live] failed: {e}")
                la = {"error": str(e)}
                rc = 2
            doc["live_awareness"] = la
            if rc == 0 and not la.get("ok"):
                rc = 1
            log(f"[live] {'pass' if la.get('ok') else 'FAIL'}")
    except Exception:
        traceback.print_exc()
        rc = 2
    finally:
        site.stop()
        _write(out, doc, time.perf_counter() - t0)
        if not a.keep and not a.work:
            shutil.rmtree(work, ignore_errors=True)
    log(f"results: {out / 'results.json'} and {out / 'summary.md'}")
    return rc


def _write(out: Path, doc: dict, elapsed: float) -> None:
    doc["elapsed_s"] = round(elapsed, 1)
    (out / "results.json").write_text(json.dumps(doc, indent=2) + "\n")
    good = dict(doc, scales=[s for s in doc["scales"] if "error" not in s])
    text = report.markdown(good)
    if "live_awareness" in doc:
        text += "\n".join(report.live_md(doc["live_awareness"])) + "\n"
    for s in doc["scales"]:
        if "error" in s:
            text += f"\n## Scale {s['scale']:,}: error\n\n```\n{s['error']}\n```\n"
    (out / "summary.md").write_text(text)


if __name__ == "__main__":
    sys.exit(main())
