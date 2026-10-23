"""Many working copies on one host (WI-2313): N graft clones of one
repository side by side, as N agents on one machine would make them, against
N git worktrees of one clone.

For each N and each shape (full, and sparse on one package) the N clones start
at once. Reported: wall time for all N, per-clone latency, and bytes on disk
split into checked-out files, `.graft/cache` and the rest of `.graft/`. git's
side is one clone plus N-1 `git worktree add`, which share one object store.

Gated (`ok`): every copy exists and its tree equals the model's slice. Times
are reported, never gated. Seeded: the repository is the generator's history
for (seed, scale), imported with graft-mirror.

Without graft-node: the harness has no way to start one locally (the node
needs the server's public key, which `graft-server admin` does not print), and
the client keeps its own `.graft/cache` even behind a node today.
"""

from __future__ import annotations

import hashlib
import shutil
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from . import gen
from .model import apply_model, compare, read_tree
from .proc import summarize
from .site import du
from .tools import Git, Graft, Mirror

SHAPES = ("full", "sparse")


def sizes(root: Path) -> dict:
    """Bytes in one working copy: checked-out files, the object cache, and
    the rest of `.graft/` (index, state, config)."""
    total = du(root)
    graft_dir = root / ".graft"
    cache = du(graft_dir / "cache")
    meta = du(graft_dir) - cache
    git_dir = root / ".git"
    git_meta = du(git_dir) if git_dir.is_dir() else (git_dir.stat().st_size if git_dir.exists() else 0)
    return {"files": total - cache - meta - git_meta, "cache": cache, "meta": meta + git_meta, "total": total}


def _sum(rows: list[dict]) -> dict:
    return {k: sum(r[k] for r in rows) for k in ("files", "cache", "meta", "total")}


def slice_of(model: dict[str, bytes], prefix: str | None) -> dict[str, bytes]:
    if prefix is None:
        return dict(model)
    return {p: v for p, v in model.items() if p.startswith(prefix + "/")}


def cone_parents(model: dict[str, bytes], prefix: str) -> dict[str, bytes]:
    """The files git's cone mode adds to a prefix: those directly in the
    root and in each parent directory of the prefix."""
    out = {}
    for p, v in model.items():
        parent = p.rsplit("/", 1)[0] if "/" in p else ""
        if parent == "" or prefix.startswith(parent + "/"):
            out[p] = v
    return out


def graft_copies(graft: Graft, repo: str, base: Path, n: int, prefix: str | None) -> tuple[float, list[float], list[Path]]:
    """`n` graft clones into `base`, all started at once."""
    dirs = [base / f"wc-{i:02d}" for i in range(n)]
    extra = ["--path", prefix] if prefix else []

    def one(d: Path) -> float:
        return graft(["clone", repo, d] + extra).secs

    t0 = time.perf_counter()
    with ThreadPoolExecutor(max_workers=n) as pool:
        lat = list(pool.map(one, dirs))
    return time.perf_counter() - t0, lat, dirs


def git_copies(git: Git, bare: Path, base: Path, n: int, prefix: str | None) -> tuple[float, list[float], list[Path]]:
    """One git clone plus `n - 1` worktrees of it (git's answer to many
    working copies: one shared object store). Worktrees are added one at a
    time, since git locks its repository for each."""
    first = base / "wt-00"
    lat = []
    t0 = time.perf_counter()
    if prefix:
        t = git(["clone", "-q", "--no-local", "--filter=blob:none", "--no-checkout", bare.as_uri(), first]).secs
        t += git(["sparse-checkout", "set", "--cone", prefix], cwd=first).secs
        t += git(["checkout", "-q", "main"], cwd=first).secs
    else:
        t = git(["clone", "-q", "--no-local", bare.as_uri(), first]).secs
    lat.append(t)
    dirs = [first]
    for i in range(1, n):
        d = base / f"wt-{i:02d}"
        args = ["worktree", "add", "-q", "--detach"]
        lat.append(git(args + ([] if not prefix else ["--no-checkout"]) + [d, "main"], cwd=first).secs)
        if prefix:
            # Each worktree has its own sparse-checkout patterns.
            lat[-1] += git(["sparse-checkout", "set", "--cone", prefix], cwd=d).secs
            lat[-1] += git(["checkout", "-q", "--detach", "main"], cwd=d).secs
        dirs.append(d)
    return time.perf_counter() - t0, lat, dirs


def _tree(root: Path) -> dict[str, bytes]:
    tree = read_tree(root)
    tree.pop(".git", None)  # a linked git worktree's pointer file
    return tree


def run_scale(cfg, scale: int, site, graft: Graft, mirror: Mirror, git: Git, repo: str, counts: list[int]) -> dict:
    log = cfg.log
    w = cfg.work / f"copies-{scale}"
    if w.exists():
        shutil.rmtree(w)
    w.mkdir(parents=True)
    res: dict = {"kind": "many_copies", "scale": scale, "seed": cfg.seed, "repo": repo, "counts": counts}

    log(f"[copies {scale}] generate {scale} commits (seed {cfg.seed}) and import")
    h = hashlib.sha256()
    model: dict[str, bytes] = {}

    def tapped():
        for c in gen.stream(cfg.seed, scale):
            gen.digest_commit(h, c)
            apply_model(model, c.ops)
            yield c

    bare = w / "ref.git"
    git.fast_import(bare, tapped())
    r = mirror.import_git(bare, repo, w / "mirror.db")
    pkgs = sorted({p.rsplit("/", 1)[0] for p in model if p.startswith(gen.SRC + "/") and p.endswith("/__init__.py")} - {gen.SRC})
    sparse = pkgs[0]
    res["repository"] = {
        "stream_sha256": h.hexdigest(),
        "files": len(model),
        "bytes": sum(len(v) for v in model.values()),
        "import_s": round(r.secs, 3),
        "sparse_prefix": sparse,
        "sparse_files": len(slice_of(model, sparse)),
        "sparse_bytes": sum(len(v) for v in slice_of(model, sparse).values()),
    }
    server_before = site.stats().get("storage_bytes")

    ok = True
    runs = []
    for shape in SHAPES:
        prefix = sparse if shape == "sparse" else None
        want = slice_of(model, prefix)
        for n in counts:
            for system in ("graft", "git"):
                base = w / f"{system}-{shape}-{n}"
                base.mkdir()
                log(f"[copies {scale}] {system} {shape} x {n}")
                make = graft_copies if system == "graft" else git_copies
                wall, lat, dirs = make(graft if system == "graft" else git, repo if system == "graft" else bare, base, n, prefix)
                each = [sizes(d) for d in dirs]
                expect = want
                if system == "git" and prefix:
                    expect = dict(want, **cone_parents(model, prefix))
                matched = sum(1 for d in dirs if compare(expect, _tree(d))["equal"])
                total = _sum(each)
                if system == "git":
                    # The shared object store lives in the first worktree's
                    # .git; count it once, as the store, not as files.
                    total["shared_store"] = du(dirs[0] / ".git")
                good = len(dirs) == n and matched == n
                ok = ok and good
                runs.append(
                    {
                        "system": system,
                        "shape": shape,
                        "n": n,
                        "copies_ok": good,
                        "trees_match": matched,
                        "wall_s": round(wall, 3),
                        "clone": summarize(lat),
                        "bytes": total,
                        "bytes_per_copy": {k: v // n for k, v in total.items()},
                    }
                )
                shutil.rmtree(base)
    res["runs"] = runs
    # Clones only read: the server's storage must not grow with N.
    res["server_storage_growth_bytes"] = (site.stats().get("storage_bytes") or 0) - (server_before or 0)
    res["node"] = "not run: graft-node needs the server's public key, which graft-server admin does not print"
    res["ok"] = ok
    return res
