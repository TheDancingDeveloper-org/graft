"""Replaying commit operations: in memory (the model), onto a working
directory, and comparing trees."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path

# Directories a tree comparison skips: the tools' own state.
TOOL_DIRS = {".git", ".graft"}


def apply_model(tree: dict[str, bytes], ops: list) -> None:
    """Applies `ops` to an in-memory tree (path -> bytes). Fails loudly on
    an operation that does not fit the tree, so a generator bug cannot hide."""
    for op in ops:
        if op[0] == "w":
            tree[op[1]] = op[2]
        elif op[0] == "d":
            del tree[op[1]]
        elif op[0] == "r":
            if op[2] in tree:
                raise ValueError(f"rename onto an existing file: {op[2]}")
            tree[op[2]] = tree.pop(op[1])
        else:
            raise ValueError(f"unknown op {op[0]!r}")


def _prune(root: Path, d: Path) -> None:
    while d != root:
        try:
            d.rmdir()
        except OSError:
            return
        d = d.parent


def apply_disk(root: Path, ops: list) -> None:
    """Applies `ops` to the files under `root`, removing directories that a
    delete or rename leaves empty."""
    for op in ops:
        if op[0] == "w":
            p = root / op[1]
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(op[2])
        elif op[0] == "d":
            p = root / op[1]
            p.unlink()
            _prune(root, p.parent)
        elif op[0] == "r":
            src, dst = root / op[1], root / op[2]
            dst.parent.mkdir(parents=True, exist_ok=True)
            os.rename(src, dst)
            _prune(root, src.parent)
        else:
            raise ValueError(f"unknown op {op[0]!r}")


def read_tree(root: Path) -> dict[str, bytes]:
    """Every file under `root` (paths relative, `/`-separated), skipping
    the tools' state directories."""
    out: dict[str, bytes] = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in TOOL_DIRS]
        for name in filenames:
            p = Path(dirpath) / name
            out[p.relative_to(root).as_posix()] = p.read_bytes()
    return out


def tree_digest(tree: dict[str, bytes]) -> str:
    h = hashlib.sha256()
    for path in sorted(tree):
        h.update(path.encode() + b"\0" + hashlib.sha256(tree[path]).digest())
    return h.hexdigest()


def compare(a: dict[str, bytes], b: dict[str, bytes], limit: int = 10) -> dict:
    """A byte-for-byte comparison: equal, plus up to `limit` examples of
    each kind of difference."""
    only_a = sorted(set(a) - set(b))
    only_b = sorted(set(b) - set(a))
    differ = sorted(p for p in set(a) & set(b) if a[p] != b[p])
    return {
        "equal": not (only_a or only_b or differ),
        "files": len(a),
        "only_left": only_a[:limit],
        "only_right": only_b[:limit],
        "differ": differ[:limit],
        "counts": [len(only_a), len(only_b), len(differ)],
    }
