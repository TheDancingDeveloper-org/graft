"""Unit tests for the live-awareness script, scheduler and oracle (WI-2311):
python3 -m unittest discover -s tests (from bench/)."""

from __future__ import annotations

import random
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from graftbench import livea  # noqa: E402
from graftbench.mcp import _decode  # noqa: E402
from graftbench.tools import Git  # noqa: E402


class ScriptTest(unittest.TestCase):
    def test_same_seed_same_script(self):
        for o in livea.OVERLAPS:
            self.assertEqual(livea.Script(7, 4, 6, o).digest(), livea.Script(7, 4, 6, o).digest())

    def test_other_seed_or_overlap_other_script(self):
        self.assertNotEqual(livea.Script(7, 4, 6, "mid").digest(), livea.Script(8, 4, 6, "mid").digest())
        self.assertNotEqual(livea.Script(7, 4, 6, "low").digest(), livea.Script(7, 4, 6, "high").digest())

    def test_markers_are_distinct(self):
        s = livea.Script(3, 5, 8, "high")
        markers = [st.marker for mine in s.steps for st in mine]
        self.assertEqual(len(markers), 40)
        self.assertEqual(len(set(markers)), 40)

    def test_overlap_raises_the_shared_share(self):
        def hot(o):
            return sum(livea.Script(seed, 6, 20, o).kinds()["hot"] for seed in range(5))

        self.assertLess(hot("low"), hot("mid"))
        self.assertLess(hot("mid"), hot("high"))

    def test_expected_tree_is_order_independent(self):
        s = livea.Script(11, 4, 6, "high")
        steps = [st for mine in s.steps for st in mine]
        for seed in range(5):
            tree = s.initial()
            random.Random(seed).shuffle(steps)
            for st in steps:
                tree[st.path] = livea.edit_text(tree[st.path], st.key, st.marker)
            self.assertEqual(tree, s.expected())

    def test_resolve_takes_the_sorted_union(self):
        text = (
            b'"""m."""\n\n<<<<<<< ours\nHOT_0_0 = "w1s0,w1s2"\n||||||| base\nHOT_0_0 = ""\n'
            b'=======\nHOT_0_0 = "w0s1"\n>>>>>>> theirs\nPAD_HOT_0_0_0 = 0\n'
        )
        out, blocks, lines = livea.resolve_text(text)
        self.assertEqual(out, b'"""m."""\n\nHOT_0_0 = "w0s1,w1s0,w1s2"\nPAD_HOT_0_0_0 = 0\n')
        self.assertEqual((blocks, lines), (1, 5))  # three sides and two separators
        self.assertEqual(livea.resolve_text(out), (out, 0, 0))


class ScheduleTest(unittest.TestCase):
    def test_deterministic_and_complete(self):
        lengths = [13, 7, 13, 1, 9]
        a = livea.schedule(5, lengths)
        self.assertEqual(a, livea.schedule(5, lengths))
        self.assertNotEqual(a, livea.schedule(6, lengths))
        self.assertEqual([a.count(w) for w in range(5)], lengths)

    def test_interleaves(self):
        a = livea.schedule(1, [10, 10, 10])
        self.assertGreater(sum(1 for x, y in zip(a, a[1:]) if x != y), 10)


class OracleTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.merger = livea.Merger(Git(), self.dir)

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def oracle(self, script, policy):
        progs = [livea.program(script, i, policy) for i in range(script.writers)]
        return livea.Oracle(script, policy, self.merger).run(livea.schedule(script.seed, [len(p) for p in progs]), progs)

    def test_merge3_reference(self):
        s = livea.Script(0, 2, 1, "low")
        p = livea.shared_path(0)
        base = s.initial()[p]
        a = livea.edit_text(base, "REGION_0_W0", "a")
        b = livea.edit_text(base, "REGION_0_W1", "b")
        merged, conflicted = self.merger.merge(base, a, b)
        self.assertFalse(conflicted)
        self.assertEqual(merged, livea.edit_text(a, "REGION_0_W1", "b"))
        c = livea.edit_text(base, "REGION_0_W0", "c")
        merged, conflicted = self.merger.merge(base, a, c)
        self.assertTrue(conflicted)
        self.assertEqual(livea.resolve_text(merged)[0], livea.edit_text(a, "REGION_0_W0", "c"))

    def test_every_policy_reaches_the_expected_tree(self):
        for o in livea.OVERLAPS:
            s = livea.Script(2294, 4, 6, o)
            for policy in ("late", "sync-early"):
                self.assertEqual(self.oracle(s, policy).tip, s.expected(), (o, policy))

    def test_same_seed_same_counts_and_events(self):
        s = livea.Script(9, 4, 6, "high")
        a, b = self.oracle(s, "late"), self.oracle(s, "late")
        self.assertEqual(a.counts, b.counts)
        self.assertEqual([e.key() for e in a.events], [e.key() for e in b.events])

    def test_conflicts_follow_overlap(self):
        def at_land(o, policy):
            return sum(self.oracle(livea.Script(seed, 4, 6, o), policy).counts.conflicts_at_land for seed in range(3))

        self.assertLessEqual(at_land("low", "late"), at_land("high", "late"))
        self.assertGreater(at_land("high", "late"), 0)
        # Syncing before each step catches some conflicts before the land.
        caught = sum(self.oracle(livea.Script(seed, 4, 6, "high"), "sync-early").counts.conflicts_caught_early for seed in range(3))
        self.assertGreater(caught, 0)

    def test_draft_paths_are_tracked(self):
        o = self.oracle(livea.Script(2294, 4, 6, "mid"), "late")
        self.assertEqual(len(o.drafts), sum(len(livea.program(o.script, i, "late")) for i in range(4)))
        # Every writer has landed at the end: nothing is left open.
        self.assertEqual(o.drafts[-1], [[] for _ in range(4)])
        self.assertGreater(o.draft_overlap()["overlap_pairs"], 0)


class McpDecodeTest(unittest.TestCase):
    def test_json_and_sse(self):
        self.assertEqual(_decode('{"id": 1}'), {"id": 1})
        self.assertEqual(_decode('event: message\ndata: {"id": 2}\n\n'), {"id": 2})
        self.assertIsNone(_decode(""))


if __name__ == "__main__":
    unittest.main()
