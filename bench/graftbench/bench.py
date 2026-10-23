"""One scale of the benchmark: generate, import, replay, measure, verify."""

from __future__ import annotations

import hashlib
import shutil
import time
from dataclasses import dataclass
from pathlib import Path

from . import conc, gen
from .model import apply_disk, apply_model, compare, read_tree, tree_digest
from .proc import StepError, summarize
from .site import du
from .tools import Git, Graft, Mirror, commit_env

STATUS_EDITS = 10
SYNC_BEHIND = 5
RANGE = 20


@dataclass
class Config:
    seed: int
    writers: int
    rounds: int
    iterations: int
    work: Path
    log: callable


def _progress(log, label: str, i: int, n: int, t0: float) -> None:
    if n >= 100 and i % max(1, n // 10) == 0 and i:
        rate = i / (time.perf_counter() - t0)
        log(f"  {label}: {i}/{n} ({rate:.1f}/s)")


def run_scale(cfg: Config, scale: int, site, graft: Graft, mirror: Mirror, git: Git, repo: str, import_repo: str | None) -> dict:
    log = cfg.log
    w = cfg.work / f"scale-{scale}"
    if w.exists():
        shutil.rmtree(w)
    w.mkdir(parents=True)
    res: dict = {"scale": scale, "seed": cfg.seed, "modules": gen.modules_for(scale), "repo": repo}
    stats = {"start": site.stats()}

    # 1. Generate: the reference git repository (fast-import), the model's
    #    final tree, the stream digest, and a one-commit seed repository.
    log(f"[{scale}] generate {scale} commits (seed {cfg.seed})")
    t0 = time.perf_counter()
    h = hashlib.sha256()
    model: dict[str, bytes] = {}
    kinds: dict[str, int] = {}
    ops_count = 0

    def tapped():
        nonlocal ops_count
        for c in gen.stream(cfg.seed, scale):
            gen.digest_commit(h, c)
            apply_model(model, c.ops)
            kinds[c.kind] = kinds.get(c.kind, 0) + 1
            ops_count += len(c.ops)
            yield c

    ref = w / "ref.git"
    git.fast_import(ref, tapped())
    gen_s = time.perf_counter() - t0
    res["generate"] = {
        "seconds": round(gen_s, 3),
        "stream_sha256": h.hexdigest(),
        "ref_head": git.head(ref, "main"),
        "commits": scale + 1,
        "ops": ops_count,
        "kinds": dict(sorted(kinds.items())),
        "final_files": len(model),
        "final_bytes": sum(len(v) for v in model.values()),
        "final_tree_sha256": tree_digest(model),
    }
    seed_git = w / "seed.git"
    git.fast_import(seed_git, [next(gen.stream(cfg.seed, scale))])

    # 2. Import throughput: graft-mirror import of the whole reference
    #    history (git's side of the same step is fast-import, above).
    if import_repo:
        log(f"[{scale}] graft-mirror import {scale + 1} commits into {import_repo}")
        r = mirror.import_git(ref, import_repo, w / "import-mirror.db")
        res["import"] = {
            "graft_mirror_import_s": round(r.secs, 3),
            "commits_per_s": round((scale + 1) / r.secs, 2),
            "git_fast_import_s": round(gen_s, 3),
            "note": "git_fast_import_s includes generating the stream",
        }
        imp_wc = w / "import-wc"
        graft(["clone", import_repo, imp_wc])
        cmp = compare(model, read_tree(imp_wc))
        res["import"]["final_tree_matches_model"] = cmp["equal"]
        if not cmp["equal"]:
            res["import"]["mismatch"] = cmp
        stats["after_import"] = site.stats()

    # 3. Seed the replay repository with commit 0 (an empty graft repository
    #    has no line yet), then replay commits 1..N through the client:
    #    edit files, `graft commit`, `graft land`.
    mirror.import_git(seed_git, repo, w / "seed-mirror.db")
    gwc = w / "graft-wc"
    graft(["clone", repo, gwc])
    seed_id = graft.json(["log", "--limit", "1"], cwd=gwc)[1]["changesets"][0]["id"]
    landed: list[str] = [seed_id]

    log(f"[{scale}] graft replay: commit + land x {scale}")
    commit_s, land_s, rebased = [], [], 0
    stream = gen.stream(cfg.seed, scale)
    first = next(stream)
    t0 = time.perf_counter()
    for i, c in enumerate(stream, 1):
        apply_disk(gwc, c.ops)
        name, email = c.author
        r = graft(["commit", "-m", c.message, "--author", f"{name} <{email}>", "--author-kind", "agent"], cwd=gwc)
        commit_s.append(r.secs)
        r, j = graft.json(["land"], cwd=gwc)
        land_s.append(r.secs)
        landed.append(j["landed"])
        rebased += bool(j.get("rebased"))
        _progress(log, "graft replay", i, scale, t0)
    replay_wall = time.perf_counter() - t0
    res["graft_replay"] = {
        "wall_s": round(replay_wall, 3),
        "commits_per_s": round(scale / replay_wall, 2) if replay_wall else None,
        "commit": summarize(commit_s),
        "land": summarize(land_s),
        "rebased": rebased,
    }
    stats["after_graft_replay"] = site.stats()

    # 4. The git baseline: the same commits through `git add -A`,
    #    `git commit` and `git push` to a bare repository.
    log(f"[{scale}] git replay: commit + push x {scale}")
    bare = w / "git-replay.git"
    git(["init", "-q", "--bare", "-b", "main", bare])
    git(["config", "uploadpack.allowFilter", "true"], cwd=bare)
    gw = w / "git-wc"
    git(["init", "-q", "-b", "main", gw])
    git(["remote", "add", "origin", bare.as_uri()], cwd=gw)
    apply_disk(gw, first.ops)
    git(["add", "-A"], cwd=gw)
    git(["commit", "-q", "-m", first.message], cwd=gw, env=commit_env(first))
    git(["push", "-q", "origin", "main"], cwd=gw)
    gcommit_s, push_s = [], []
    t0 = time.perf_counter()
    for i, c in enumerate(gen.stream(cfg.seed, scale)):
        if i == 0:
            continue
        apply_disk(gw, c.ops)
        a = git(["add", "-A"], cwd=gw)
        b = git(["commit", "-q", "-m", c.message], cwd=gw, env=commit_env(c))
        gcommit_s.append(a.secs + b.secs)
        push_s.append(git(["push", "-q", "origin", "main"], cwd=gw).secs)
        _progress(log, "git replay", i, scale, t0)
    gwall = time.perf_counter() - t0
    res["git_replay"] = {
        "wall_s": round(gwall, 3),
        "commits_per_s": round(scale / gwall, 2) if gwall else None,
        "commit": summarize(gcommit_s),
        "push": summarize(push_s),
        "repo_bytes": du(bare),
    }

    # 5. Correctness of the replay.
    log(f"[{scale}] verify replay")
    git_head = git.head(bare, "main")
    graft_tree = read_tree(gwc)
    git_tree = read_tree(gw)
    log_ids = list(reversed(graft.log_ids(gwc)))
    res["verify_replay"] = {
        "git_replay_head_equals_fast_import_head": git_head == res["generate"]["ref_head"],
        "graft_tree_vs_model": _brief(compare(model, graft_tree)),
        "git_tree_vs_model": _brief(compare(model, git_tree)),
        "graft_tree_vs_git_tree": _brief(compare(git_tree, graft_tree)),
        "graft_log_length": len(log_ids),
        "graft_log_equals_landed_sequence": log_ids == landed,
        "landed_unique": len(set(landed)) == len(landed),
    }

    # 6. Single-agent operations, graft against git.
    log(f"[{scale}] operations x {cfg.iterations}")
    res["ops"] = ops_bench(cfg, w, graft, git, repo, bare, gwc, gw, model, landed)
    stats["after_ops"] = site.stats()

    # 7. Concurrent writers.
    log(f"[{scale}] concurrent writers: {cfg.writers} x {cfg.rounds} rounds")
    modules = sorted(p for p in model if p.startswith(gen.SRC + "/") and p.endswith(".py") and not p.endswith("__init__.py"))
    plan = conc.Plan(modules, cfg.writers, cfg.rounds)
    setup = plan.setup_files()
    graft(["sync"], cwd=gwc)
    apply_disk(gwc, [("w", p, d) for p, d in setup.items()])
    graft(["commit", "-m", "bench: concurrent scenario files"], cwd=gwc)
    landed.append(graft.json(["land"], cwd=gwc)[1]["landed"])
    git(["pull", "-q", "--ff-only", "origin", "main"], cwd=gw)
    apply_disk(gw, [("w", p, d) for p, d in setup.items()])
    git(["add", "-A"], cwd=gw)
    git(["commit", "-q", "-m", "bench: concurrent scenario files"], cwd=gw)
    git(["push", "-q", "origin", "main"], cwd=gw)

    gwcs, gitwcs = [], []
    for i in range(cfg.writers):
        d = w / f"graft-writer-{i}"
        graft(["clone", repo, d])
        gwcs.append(d)
        d = w / f"git-writer-{i}"
        git(["clone", "-q", bare.as_uri(), d])
        gitwcs.append(d)
    conc_landed: list[str] = []
    with conc.TipWatcher(graft, gwc) as watcher:
        graft_conc = conc.run_graft(graft, gwcs, plan, conc_landed)
    res["concurrent"] = {
        "writers": cfg.writers,
        "rounds": cfg.rounds,
        "graft": graft_conc,
        "git": conc.run_git(git, gitwcs, plan),
    }
    landed += conc_landed
    stats["after_concurrent"] = site.stats()

    # 8. Final correctness: both systems hold the expected tree, byte for
    #    byte, and graft's line holds every land exactly once, in order.
    log(f"[{scale}] verify final trees and history")
    expected = dict(model)
    expected.update(res["ops"].pop("_extra_files"))
    plan.expected(expected)
    final_g = w / "final-graft"
    graft(["clone", repo, final_g])
    final_git = w / "final-git"
    git(["clone", "-q", bare.as_uri(), final_git])
    g_tree, git_tree = read_tree(final_g), read_tree(final_git)
    log_ids = list(reversed(graft.log_ids(final_g)))
    res["verify_final"] = {
        "graft_vs_expected": _brief(compare(expected, g_tree)),
        "git_vs_expected": _brief(compare(expected, git_tree)),
        "graft_vs_git": _brief(compare(git_tree, g_tree)),
        "graft_log_length": len(log_ids),
        "lands_recorded": len(landed),
        "no_lost_or_duplicated_lands": sorted(log_ids) == sorted(landed) and len(set(log_ids)) == len(log_ids),
        "replay_lands_in_order": _is_subsequence(landed[: scale + 1], log_ids),
        "tips_observed": len(watcher.tips),
        "line_never_moved_backwards": _is_subsequence(landed[: scale + 1], log_ids)
        and bool(watcher.tips)
        and _is_subsequence(watcher.tips, log_ids),
    }
    res["verify_final"]["blame"] = [blame_check(graft, git, final_g, final_git, p, set(log_ids)) for p in (gen.REGISTRY, gen.CHANGELOG)]
    stats["end"] = site.stats()
    res["server"] = stats
    res["ok"] = _all_ok(res)
    return res


def blame_check(graft: Graft, git: Git, gdir: Path, gitdir: Path, path: str, on_line: set) -> dict:
    """Blame is intact when its ranges cover the file exactly and every
    origin is on the line. Agreement with git is by commit subject per line
    (both histories carry the same messages); equal-cost tie-breaks can
    differ, so it is reported, not required."""
    _, j = graft.json(["blame", path, "--at", "main"], cwd=gdir)
    lines = (gdir / path).read_bytes().count(b"\n")
    subjects: list[str] = []
    pos, intact = 0, True
    for r in j.get("ranges", []):
        intact &= r["start"] == pos and r["origin"]["id"] in on_line
        subjects += [r["origin"]["subject"]] * (r["end"] - r["start"])
        pos = r["end"]
    intact &= pos == lines
    out = git(["blame", "--line-porcelain", "main", "--", path], cwd=gitdir).out
    git_subjects = [x[len("summary ") :] for x in out.splitlines() if x.startswith("summary ")]
    agree = sum(1 for a, b in zip(subjects, git_subjects) if a == b)
    return {"path": path, "lines": lines, "intact": intact, "agree_with_git": agree}


def _brief(cmp: dict) -> dict:
    if cmp["equal"]:
        return {"equal": True, "files": cmp["files"]}
    return cmp


def _is_subsequence(seq: list[str], of: list[str]) -> bool:
    """`seq` appears on the line in order. A line that moved backwards would
    drop one of them or reorder two."""
    pos = {x: i for i, x in enumerate(of)}
    last = -1
    for x in seq:
        if x not in pos or pos[x] <= last:
            return False
        last = pos[x]
    return True


def _all_ok(res: dict) -> bool:
    v, f = res["verify_replay"], res["verify_final"]
    checks = [
        v["git_replay_head_equals_fast_import_head"],
        v["graft_tree_vs_model"]["equal"],
        v["git_tree_vs_model"]["equal"],
        v["graft_log_equals_landed_sequence"],
        f["graft_vs_expected"]["equal"],
        f["git_vs_expected"]["equal"],
        f["no_lost_or_duplicated_lands"],
        f["line_never_moved_backwards"],
        all(b["intact"] for b in f["blame"]),
        res.get("import", {}).get("final_tree_matches_model", True),
    ]
    for side in ("graft", "git"):
        for s in conc.SCENARIOS:
            checks.append(res["concurrent"][side][s]["failed"] == 0)
        checks.append(res["concurrent"][side]["conflict"]["conflicts_surfaced"] >= res["concurrent"]["rounds"])
        checks.append(res["concurrent"][side]["merge3"]["conflicts_surfaced"] == 0)
    return all(checks)


def _timed(samples: list, fn) -> None:
    t0 = time.perf_counter()
    fn()
    samples.append(time.perf_counter() - t0)


def ops_bench(
    cfg: Config, w: Path, graft: Graft, git: Git, repo: str, bare: Path, gwc: Path, gw: Path, model: dict, landed: list[str]
) -> dict:
    n = cfg.iterations
    pkgs = sorted({p.rsplit("/", 1)[0] for p in model if p.startswith(gen.SRC + "/") and p.endswith("/__init__.py")} - {gen.SRC})
    sparse = pkgs[0]
    hot = gen.REGISTRY
    s: dict[str, dict[str, list]] = {}

    def add(op, system, secs):
        s.setdefault(op, {}).setdefault(system, []).append(secs)

    scratch = w / "ops"
    scratch.mkdir()
    for k in range(n):
        d = scratch / f"g-full-{k}"
        add("clone_full", "graft", graft(["clone", repo, d]).secs)
        if k:
            shutil.rmtree(d)
        d = scratch / f"g-sparse-{k}"
        add("clone_sparse", "graft", graft(["clone", repo, d, "--path", sparse]).secs)
        shutil.rmtree(d)
        d = scratch / f"git-full-{k}"
        add("clone_full", "git", git(["clone", "-q", "--no-local", bare.as_uri(), d]).secs)
        if k:
            shutil.rmtree(d)
        d = scratch / f"git-sparse-{k}"
        t = git(["clone", "-q", "--no-local", "--filter=blob:none", "--no-checkout", bare.as_uri(), d]).secs
        t += git(["sparse-checkout", "set", "--cone", sparse], cwd=d).secs
        t += git(["checkout", "-q", "main"], cwd=d).secs
        add("clone_sparse", "git", t)
        shutil.rmtree(d)

    # Working-copy reads on the first full clones, with some local edits.
    gc, gitc = scratch / "g-full-0", scratch / "git-full-0"
    mods = sorted(p for p in model if p.endswith(".py") and p.startswith(gen.SRC + "/"))
    edits = mods[:: max(1, len(mods) // STATUS_EDITS)][:STATUS_EDITS]
    for root in (gc, gitc):
        for p in edits:
            f = root / p
            f.write_bytes(f.read_bytes() + b"\n# local edit\n")
        (root / "scratch_new.py").write_text("NEW = 1\n")
    ids_range = (landed[max(0, len(landed) - 1 - RANGE)], landed[-1])
    span = min(RANGE, len(landed) - 1)
    for _ in range(n):
        add("status", "graft", graft(["status"], cwd=gc).secs)
        add("status", "git", git(["status", "--porcelain"], cwd=gitc).secs)
        add("diff_worktree", "graft", graft(["diff"], cwd=gc).secs)
        add("diff_worktree", "git", git(["diff"], cwd=gitc).secs)
        add("diff_range", "graft", graft(["diff", ids_range[0], ids_range[1]], cwd=gc).secs)
        add("diff_range", "git", git(["diff", f"main~{span}", "main"], cwd=gitc).secs)
        add("log", "graft", graft(["log", "--limit", "50"], cwd=gc).secs)
        add("log", "git", git(["log", "-50", "main"], cwd=gitc).secs)
        add("log_path", "graft", graft(["log", "--limit", "50", "--path", hot], cwd=gc).secs)
        add("log_path", "git", git(["log", "-50", "main", "--", hot], cwd=gitc).secs)
        add("blame", "graft", graft(["blame", hot], cwd=gc).secs)
        add("blame", "git", git(["blame", "main", "--", hot], cwd=gitc).secs)

    # Sync: a working copy behind by SYNC_BEHIND lands catches up.
    tick = "bench_sync/tick.txt"
    g_sync, git_sync = scratch / "g-sync", scratch / "git-sync"
    graft(["clone", repo, g_sync])
    git(["clone", "-q", bare.as_uri(), git_sync])
    count = 0
    for _ in range(n):
        for _ in range(SYNC_BEHIND):
            count += 1
            data = f"tick {count}\n".encode()
            apply_disk(gwc, [("w", tick, data)])
            graft(["sync"], cwd=gwc)
            graft(["commit", "-m", f"bench: tick {count}"], cwd=gwc)
            landed.append(graft.json(["land"], cwd=gwc)[1]["landed"])
            apply_disk(gw, [("w", tick, data)])
            git(["add", "-A"], cwd=gw)
            git(["commit", "-q", "-m", f"bench: tick {count}"], cwd=gw)
            git(["push", "-q", "origin", "main"], cwd=gw)
        add("sync", "graft", graft(["sync"], cwd=g_sync).secs)
        add("sync", "git", git(["pull", "-q", "--ff-only", "origin", "main"], cwd=git_sync).secs)
        if (g_sync / tick).read_bytes() != data or (git_sync / tick).read_bytes() != data:
            raise StepError("sync did not bring the working copy to the tip")

    out: dict = {
        "sparse_prefix": sparse,
        "status_edits": STATUS_EDITS + 1,
        "diff_range_changesets": span,
        "sync_behind": SYNC_BEHIND,
        "hot_path": hot,
    }
    for op, systems in s.items():
        out[op] = {system: summarize(v) for system, v in systems.items()}
        g50 = out[op].get("graft", {}).get("p50_ms")
        t50 = out[op].get("git", {}).get("p50_ms")
        out[op]["graft_over_git_p50"] = round(g50 / t50, 2) if g50 and t50 else None
    out["_extra_files"] = {tick: f"tick {count}\n".encode()} if count else {}
    return out
