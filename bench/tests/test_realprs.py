"""Unit tests for the real-PR replay's helpers (WI-2338):
python3 -m unittest discover -s tests (from bench/)."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from graftbench import realprs  # noqa: E402
from graftbench.realprs import Land  # noqa: E402


def commit(oid, *parents):
    return {"commit": {"oid": oid, "parents": {"nodes": [{"oid": p} for p in parents]}}}


def pr(n, commits, head="h", pushes=(), state="MERGED", created="2026-10-19T01:00:00Z", merged="2026-10-19T02:00:00Z", files=()):
    return {
        "number": n,
        "headRefOid": head,
        "state": state,
        "createdAt": created,
        "mergedAt": merged,
        "mergeCommit": None,
        "commits": {"nodes": list(commits)},
        "files": {"nodes": [{"path": f} for f in files]},
        "timelineItems": {"nodes": [{"createdAt": t, "beforeCommit": {"oid": o}} for t, o in pushes]},
    }


class OriginalHeadTest(unittest.TestCase):
    def test_oldest_force_push_wins(self):
        p = pr(1, [commit("c1", "f")], pushes=[("2026-10-19T03:00:00Z", "late"), ("2026-10-19T02:00:00Z", "early")])
        self.assertEqual(realprs.original_head(p), ("early", "before-force-push"))

    def test_before_the_first_merge_back(self):
        p = pr(1, [commit("c1", "f"), commit("m1", "c1", "main1"), commit("c2", "m1")], head="c2")
        self.assertEqual(realprs.original_head(p), ("c1", "before-merge-back"))
        self.assertEqual(realprs.first_merge(p)["oid"], "m1")

    def test_plain_head(self):
        self.assertEqual(realprs.original_head(pr(1, [commit("c1", "f")], head="c1")), ("c1", "head"))


class SelectTest(unittest.TestCase):
    def test_window_and_touches(self):
        prs = {
            1: pr(1, [], created="2026-10-19T01:00:00Z", merged="2026-10-19T05:00:00Z", files=["CHANGELOG.md"]),
            2: pr(2, [], created="2026-10-19T02:00:00Z", merged="2026-10-19T03:00:00Z", files=["src/a.rs"]),
            3: pr(3, [], created="2026-10-19T04:00:00Z", merged="2026-10-19T04:30:00Z", files=["CHANGELOG.md"]),
            4: pr(4, [], created="2026-10-20T04:00:00Z", merged="2026-10-20T04:30:00Z", files=["CHANGELOG.md"]),
        }
        case = {"mode": "position", "select": {"window_of": [1], "touches": ["CHANGELOG.md"], "state": "MERGED"}}
        self.assertEqual([p["number"] for p in realprs.select(case, prs)], [3, 1])

    def test_chain_keeps_order(self):
        prs = {n: pr(n, []) for n in (5, 3, 9)}
        self.assertEqual([p["number"] for p in realprs.select({"mode": "stack", "select": {"chain": [9, 3, 5]}}, prs)], [9, 3, 5])

    def test_merged_back(self):
        prs = {1: pr(1, [commit("m", "a", "b")]), 2: pr(2, [commit("c", "a")]), 3: pr(3, [commit("m", "a", "b")], merged=None)}
        case = {"mode": "position", "select": {"merged_back": True}}
        self.assertEqual([p["number"] for p in realprs.select(case, prs)], [1])


class ParseTest(unittest.TestCase):
    def test_raw_diff(self):
        out = ":100644 100644 aaa bbb M\0src/a.rs\0:000000 100755 000 ccc A\0run.sh\0:100644 000000 ddd 000 D\0old.txt\0"
        self.assertEqual(
            realprs.parse_raw(out),
            [("M", "100644", "bbb", "src/a.rs"), ("A", "100755", "ccc", "run.sh"), ("D", "000000", "000", "old.txt")],
        )

    def test_merge_tree(self):
        self.assertEqual(realprs.parse_merge_tree("t1\na.rs\nb.rs\n"), ("t1", ["a.rs", "b.rs"]))
        self.assertEqual(realprs.parse_merge_tree("t1\n"), ("t1", []))

    def test_error_summary_keeps_no_source_text(self):
        err = "src/a.rs:3:5: error[E0425]: cannot find value `secret_thing`\nsrc/a.rs:9:1: error[E0425]: x\nwarning: y\n"
        self.assertEqual(realprs.error_summary(err), ["src/a.rs: error[E0425]"])


class TallyTest(unittest.TestCase):
    def lands(self):
        return [
            Land(1, "graft", "merged", 1, 1, 0.1, tree="t", oracle="pass"),
            Land(1, "git", "merged", 3, 1, 0.2, tree="t", oracle="pass"),
            Land(2, "graft", "merged", 1, 1, 0.1, tree="u", oracle="broken"),
            Land(2, "git", "conflict", 2, 1, 0.2, conflicts=["a"]),
            Land(3, "graft", "conflict", 2, 1, 0.1, conflicts=["b"]),
            Land(3, "git", "conflict", 2, 1, 0.2, conflicts=["b"]),
        ]

    def test_tally(self):
        g = realprs.tally(self.lands(), "graft")
        self.assertEqual((g["merged_clean"], g["conflicts"], g["broken"], g["round_trips"]), (2, 1, 1, 4))

    def test_agreement(self):
        a = realprs.agreement(self.lands())
        self.assertEqual((a["both_clean"], a["graft_only_clean"], a["both_conflict"], a["same_tree"]), (1, 1, 1, 1))

    def test_markdown_renders(self):
        lands = self.lands()
        c = {
            "id": "x",
            "mode": "position",
            "what": "w",
            "n_prs": 3,
            "graft": realprs.tally(lands, "graft"),
            "git": realprs.tally(lands, "git"),
        }
        c["agreement"] = realprs.agreement(lands)
        c["lands"] = [x.doc() for x in lands]
        md = realprs.markdown({"label": "t", "started": "s", "elapsed_s": 1, "oracle": True, "cases": [c]})
        self.assertIn("| x | position | 3 | 2 | 1 | 1 | 2 |", md)
        self.assertIn("Clean but broken: #2 (graft).", md)


def g(repo, *args):
    return subprocess.run(
        ["git", "-C", str(repo), *args], check=True, capture_output=True, text=True, env=realprs.GIT_ENV | {"PATH": "/usr/bin:/bin"}
    ).stdout.strip()


class GitSideTest(unittest.TestCase):
    """The git half of a replay on a tiny repository: a clean rebase and a conflict."""

    def test_clean_then_conflict(self):
        with tempfile.TemporaryDirectory() as t:
            t = Path(t)
            src = t / "src"
            g(t, "init", "-q", "-b", "main", str(src))
            (src / "f.txt").write_text("a\nb\nc\nd\ne\n")
            g(src, "add", "-A")
            g(src, "commit", "-q", "-m", "base")
            base = g(src, "rev-parse", "HEAD")

            def branch(text):
                g(src, "checkout", "-q", "--detach", base)
                (src / "f.txt").write_text(text)
                g(src, "commit", "-q", "-am", "x")
                return g(src, "rev-parse", "HEAD")

            first = branch("A\nb\nc\nd\ne\n")
            clean = branch("a\nb\nc\nd\nE\n")
            clash = branch("Z\nb\nc\nd\ne\n")
            bare = t / "src.git"
            g(t, "clone", "-q", "--bare", str(src), str(bare))
            for c in (first, clean, clash):
                g(src, "push", "-q", str(bare), f"{c}:refs/heads/keep-{c[:7]}")

            class C:
                work = t / "work"
                log = staticmethod(lambda m: None)

            case = {"id": "tiny", "mode": "sequence", "target": base, "prs": []}
            rp = realprs.Replay(C, case, bare)
            g(t, "init", "-q", "--bare", str(rp.remote))
            g(bare, "push", "-q", str(rp.remote), f"{base}:refs/heads/main")
            g(t, "clone", "-q", "--shared", "--no-checkout", str(bare), str(rp.gw))
            g(rp.gw, "remote", "add", "r", str(rp.remote))
            outs = []
            for c in (first, clean, clash):
                cm = rp.squash(base, c, c[:7])
                outs.append(rp.git_land(c[:7], "main", [cm], base))
            self.assertEqual([o.outcome for o in outs], ["ff", "merged", "conflict"])
            self.assertEqual(outs[1].round_trips, 3)
            self.assertEqual(outs[2].conflicts, ["f.txt"])
            self.assertEqual(g(rp.remote, "show", "main:f.txt"), "A\nb\nc\nd\nE")


if __name__ == "__main__":
    unittest.main()
