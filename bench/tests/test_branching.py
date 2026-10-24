"""Unit tests for the branching-strategy script, model and resolution rule
(WI-2362): python3 -m unittest discover -s tests (from bench/)."""

from __future__ import annotations

import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from graftbench import branching as b  # noqa: E402
from graftbench.tools import Git  # noqa: E402


class ScriptTest(unittest.TestCase):
    def test_same_seed_same_script(self):
        for s in b.STRATEGIES:
            self.assertEqual(b.script(s, 2362), b.script(s, 2362))
            self.assertNotEqual(b.script(s, 2362), b.script(s, 2363))

    def test_markers_are_distinct(self):
        for s in b.STRATEGIES:
            ms = [e.marker for op in b.script(s, 2362) for c in op.commits for e in c]
            self.assertEqual(len(ms), len(set(ms)))

    def test_model_lines_and_tags(self):
        lines, tags = b.model(b.script("gitflow", 2362))
        self.assertEqual(set(lines), {"main", "develop", "release/1.0", "release/1.1"})
        self.assertEqual(set(tags), {"v1.0.0", "v1.0.1", "v1.1.0", "v1.1.1"})
        # a release merged into main carries every release marker
        rel = lines["release/1.0"]
        for p, ks in rel.items():
            for k, v in ks.items():
                self.assertLessEqual(v, lines["main"][p][k] | lines["develop"][p][k])


class ResolveTest(unittest.TestCase):
    def test_diff3_block_takes_ours_plus_theirs_new(self):
        text = b'<<<<<<< o\nk0 = "a,c"\n||||||| b\nk0 = "a,p"\n=======\nk0 = "a,p,x"\n>>>>>>> t\n'
        out, blocks = b.resolve_text(text)
        self.assertEqual((out, blocks), (b'k0 = "a,c,x"\n', 1))

    def test_merge_block_is_the_union(self):
        out, _ = b.resolve_text(b'<<<<<<< o\nk1 = "a"\n=======\nk1 = "b"\n>>>>>>> t\n')
        self.assertEqual(out, b'k1 = "a,b"\n')

    @unittest.skipUnless(shutil.which("git"), "git not installed")
    def test_pick_by_rule_matches_the_model(self):
        """A cherry-pick resolved by rule adds only the picked markers."""
        d = Path(tempfile.mkdtemp())
        try:
            p = b.mod_path(0)
            base = b.render(p, {"k0": {"m1"}})
            theirs = b.edit_text(base, "k0", "fix")
            ours = b.render(p, {"k0": {"r1"}})
            merged, clash = b.merge3(Git(), d, ours, base, theirs)
            self.assertTrue(clash)
            out, _ = b.resolve_text(merged)
            self.assertEqual(out, b.render(p, {"k0": {"r1", "fix"}}))
        finally:
            shutil.rmtree(d)


if __name__ == "__main__":
    unittest.main()
