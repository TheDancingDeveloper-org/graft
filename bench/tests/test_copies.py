"""Unit tests for the many-working-copies scenario's helpers (WI-2313):
python3 -m unittest discover -s tests (from bench/)."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from graftbench import copies  # noqa: E402
from graftbench.report import copies_md  # noqa: E402


class SizesTest(unittest.TestCase):
    def test_splits_files_cache_and_state(self):
        with tempfile.TemporaryDirectory() as t:
            root = Path(t)
            (root / "src").mkdir()
            (root / "src" / "a.py").write_bytes(b"x" * 10)
            (root / ".graft" / "cache").mkdir(parents=True)
            (root / ".graft" / "cache" / "obj").write_bytes(b"c" * 100)
            (root / ".graft" / "index").write_bytes(b"i" * 7)
            self.assertEqual(copies.sizes(root), {"files": 10, "cache": 100, "meta": 7, "total": 117})

    def test_a_worktree_pointer_file_is_state_not_a_file(self):
        with tempfile.TemporaryDirectory() as t:
            root = Path(t)
            (root / "a.py").write_bytes(b"x" * 3)
            (root / ".git").write_bytes(b"gitdir: /x\n")
            s = copies.sizes(root)
            self.assertEqual((s["files"], s["meta"]), (3, 11))
            self.assertEqual(copies._tree(root), {"a.py": b"xxx"})

    def test_slice(self):
        model = {"src/p/a.py": b"1", "src/pq/b.py": b"2", "README.md": b"3"}
        self.assertEqual(copies.slice_of(model, "src/p"), {"src/p/a.py": b"1"})
        self.assertEqual(copies.slice_of(model, None), model)

    def test_cone_parents(self):
        model = {"README.md": b"1", "src/x.py": b"2", "src/p/a.py": b"3", "src/q/b.py": b"4", "docs/d.md": b"5"}
        self.assertEqual(sorted(copies.cone_parents(model, "src/p")), ["README.md", "src/x.py"])


class ReportTest(unittest.TestCase):
    def test_renders_a_row_per_run(self):
        b = {"files": 1 << 20, "cache": 1 << 20, "meta": 0, "total": 2 << 20}
        r = {
            "scale": 10,
            "ok": True,
            "repository": {"files": 5, "bytes": 1 << 20, "sparse_prefix": "src/p", "sparse_files": 1, "sparse_bytes": 10},
            "runs": [
                {
                    "system": s,
                    "shape": "full",
                    "n": 8,
                    "copies_ok": True,
                    "wall_s": 1.0,
                    "clone": {"p50_ms": 5.0, "p95_ms": 9.0},
                    "bytes": b,
                    "bytes_per_copy": b,
                }
                for s in ("graft", "git")
            ],
            "server_storage_growth_bytes": 0,
            "node": "not run",
        }
        text = "\n".join(copies_md(r))
        self.assertEqual(text.count("| full | 8 |"), 2)


if __name__ == "__main__":
    unittest.main()
