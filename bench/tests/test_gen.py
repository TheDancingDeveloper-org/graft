"""Unit tests for the generator and the model (stdlib unittest; pytest runs
them too): python3 -m unittest discover -s tests (from bench/)."""

from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from graftbench import conc, gen  # noqa: E402
from graftbench.model import apply_disk, apply_model, compare, read_tree, tree_digest  # noqa: E402
from graftbench.tools import Git, commit_env  # noqa: E402


class GeneratorTest(unittest.TestCase):
    def test_same_seed_same_stream(self):
        self.assertEqual(gen.stream_digest(7, 300), gen.stream_digest(7, 300))

    def test_other_seed_other_stream(self):
        self.assertNotEqual(gen.stream_digest(7, 50), gen.stream_digest(8, 50))

    def test_counts_and_kinds(self):
        commits = list(gen.stream(3, 2000))
        self.assertEqual(len(commits), 2001)
        self.assertEqual([c.index for c in commits], list(range(2001)))
        kinds = {c.kind for c in commits[1:]}
        self.assertEqual(kinds, {k for k, _ in gen.KINDS})

    def test_initial_module_count(self):
        self.assertEqual(gen.modules_for(10), 200)
        self.assertEqual(gen.modules_for(100_000), 2000)
        first = next(gen.stream(1, 10))
        mods = [op for op in first.ops if op[1].endswith(".py") and not op[1].endswith("__init__.py")]
        self.assertGreaterEqual(len(mods), 200)

    def test_every_commit_changes_the_tree_and_fits_it(self):
        tree: dict[str, bytes] = {}
        for c in gen.stream(11, 1500):
            before = tree_digest(tree)
            apply_model(tree, c.ops)  # raises on a delete or rename that does not fit
            self.assertNotEqual(before, tree_digest(tree), c.message)

    def test_generator_state_matches_model(self):
        g = gen.Generator(5, 800)
        tree: dict[str, bytes] = {}
        apply_model(tree, g.initial().ops)
        for _ in range(800):
            apply_model(tree, g.next_commit().ops)
        self.assertEqual(tree, g.files)


class DiskTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())

    def tearDown(self):
        shutil.rmtree(self.dir)

    def test_disk_replay_equals_model(self):
        tree: dict[str, bytes] = {}
        for c in gen.stream(2, 400):
            apply_model(tree, c.ops)
            apply_disk(self.dir, c.ops)
        self.assertTrue(compare(tree, read_tree(self.dir))["equal"])

    @unittest.skipUnless(shutil.which("git"), "git not installed")
    def test_git_history_is_byte_identical(self):
        git = Git()
        a, b = self.dir / "a.git", self.dir / "b.git"
        git.fast_import(a, gen.stream(9, 60))
        git.fast_import(b, gen.stream(9, 60))
        self.assertEqual(git.head(a, "main"), git.head(b, "main"))
        # Committing the same stream through a working tree gives the same ids.
        wc = self.dir / "wc"
        git(["init", "-q", "-b", "main", wc])
        for c in gen.stream(9, 60):
            apply_disk(wc, c.ops)
            git(["add", "-A"], cwd=wc)
            git(["commit", "-q", "-m", c.message], cwd=wc, env=commit_env(c))
        self.assertEqual(git.head(wc), git.head(a, "main"))


class ConflictRuleTest(unittest.TestCase):
    def test_resolution_is_the_sorted_union(self):
        d = Path(tempfile.mkdtemp())
        try:
            p = d / "f.py"
            p.write_text(
                '"""x"""\n\nBEFORE = 1\n<<<<<<< ours\nVALUE = "w3"\n||||||| base\nVALUE = ""\n'
                '=======\nVALUE = "w0,w1"\n>>>>>>> theirs\nAFTER = 2\n'
            )
            self.assertTrue(conc.resolve_conflict(p))
            self.assertIn('VALUE = "w0,w1,w3"\n', p.read_text())
            self.assertFalse(conc.resolve_conflict(p))
        finally:
            shutil.rmtree(d)

    def test_merge_regions_merge_cleanly_with_git(self):
        if not shutil.which("git"):
            self.skipTest("git not installed")
        d = Path(tempfile.mkdtemp())
        try:
            base, a, b = d / "base", d / "a", d / "b"
            base.write_bytes(conc.merge_file(0, 4))
            a.write_bytes(conc.merge_file(0, 4, {1: "w1"}))
            b.write_bytes(conc.merge_file(0, 4, {2: "w2"}))
            r = subprocess.run(["git", "merge-file", "-p", a, base, b], capture_output=True)
            self.assertEqual(r.returncode, 0)
            self.assertEqual(r.stdout, conc.merge_file(0, 4, {1: "w1", 2: "w2"}))
        finally:
            shutil.rmtree(d)


if __name__ == "__main__":
    unittest.main()
