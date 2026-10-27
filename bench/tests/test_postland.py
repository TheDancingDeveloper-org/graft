"""Recorder, planter and report. Fixtures only: no sleep, no network."""

from __future__ import annotations

import json
import subprocess
import sys
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from postland import plant, record, report  # noqa: E402


def _entry(lsn, kind, body):
    return {"lsn": lsn, "kind": kind, "body": json.dumps(body)}


class RecordTest(unittest.TestCase):
    def test_observe_stamps_clock_and_flags_late(self):
        entries = [
            _entry(4, "RefMoved", {"pos": 7, "changeset": "aaa"}),
            _entry(5, "RefMoved", {"pos": 8, "changeset": "bbb"}),
            _entry(6, "Other", {"pos": 9}),
        ]
        rows = record.observe(entries, stored_lsn=4, now="2026-10-27T00:00:00Z")
        self.assertEqual([r["changeset"] for r in rows], ["aaa", "bbb"])
        self.assertTrue(rows[0]["late"])
        self.assertFalse(rows[1]["late"])
        self.assertEqual(rows[0]["observed_at"], "2026-10-27T00:00:00Z")

    def test_resume_writes_no_duplicate(self):
        out = Path(self.id().replace(".", "_") + ".jsonl")
        self.addCleanup(out.unlink, missing_ok=True)
        seen: set[int] = set()
        rows = record.observe([_entry(1, "RefMoved", {"pos": 1, "changeset": "c1"})], stored_lsn=0, now="2026-10-27T00:00:00Z")
        self.assertEqual(len(record.append_rows(out, rows, seen)), 1)
        self.assertEqual(record.append_rows(out, rows, seen), [])
        self.assertEqual(len(out.read_text(encoding="utf-8").splitlines()), 1)

    def test_run_once_against_fixture(self):
        tmp = Path(self.id().replace(".", "_"))
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        tmp.mkdir()
        journal = {
            "entries": [
                _entry(1, "RefMoved", {"pos": 3, "changeset": "land1"}),
                _entry(2, "Attested", {"pos": 3, "changeset": "land1"}),
            ],
            "next_lsn": 3,
        }
        checks = {"rows": [{"pos": 3, "changeset": "land1", "state": "good", "revert": None}]}
        stub = tmp / "graft"
        stub.write_text(
            textwrap.dedent(
                f"""\
                #!/bin/sh
                case "$2" in
                  watch) cat >/dev/null; printf '%s' '{json.dumps(journal)}' ;;
                  get_land_checks) cat >/dev/null; printf '%s' '{json.dumps(checks)}' ;;
                  *) echo "unexpected $2" >&2; exit 9 ;;
                esac
                """
            ),
            encoding="utf-8",
        )
        stub.chmod(0o755)
        out = tmp / "log.jsonl"
        state = tmp / "state.json"
        first = record.run_once(graft=str(stub), repo="graft", line="backlog-1", state_path=state, out_path=out, now="2026-10-27T01:00:00Z", page_limit=50)
        self.assertGreater(first, 0)
        again = record.run_once(graft=str(stub), repo="graft", line="backlog-1", state_path=state, out_path=out, now="2026-10-27T02:00:00Z", page_limit=50)
        self.assertEqual(again, 0)
        saved = json.loads(state.read_text(encoding="utf-8"))
        self.assertEqual(saved["lsn"], 3)
        lines = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(len(lines), len({r["lsn"] for r in lines}))
        self.assertIn("good", {r.get("state") for r in lines})


class PlantTest(unittest.TestCase):
    def test_fmt_and_test_print_both_ids(self):
        for variant, needle in (("fmt", "let value=1;"), ("test", "panic!()")):
            tmp = Path(self.id().replace(".", "_") + variant)
            self.addCleanup(lambda p=tmp: __import__("shutil").rmtree(p, ignore_errors=True))
            tmp.mkdir()
            calls = tmp / "calls"
            stub = tmp / "graft"
            stub.write_text(
                textwrap.dedent(
                    f"""\
                    #!/bin/sh
                    printf '%s\\n' "$*" >> "{calls}"
                    if [ "$1" = land ]; then
                      wc -l < "{calls}" | tr -d ' '
                    fi
                    """
                ),
                encoding="utf-8",
            )
            stub.chmod(0o755)
            plant_id, follower_id = plant.land_pair(root=tmp, variant=variant, graft=str(stub), env={})
            self.assertNotEqual(plant_id, follower_id)
            body = (tmp / plant.PLANTED).read_text(encoding="utf-8")
            self.assertIn(needle, body)
            self.assertTrue(body.endswith(plant.FOLLOWER))
            log = calls.read_text(encoding="utf-8")
            self.assertEqual(sum(1 for line in log.splitlines() if line.split()[0] == "land"), 2)
            self.assertIn(plant.PLANT_WORK_ITEM, log)


class ReportTest(unittest.TestCase):
    ROWS = [
        {"lsn": 1, "kind": "RefMoved", "pos": 1, "changeset": "p", "observed_at": "2026-10-01T00:00:00Z", "late": False},
        {"lsn": 2, "kind": "LandCheck", "pos": 1, "changeset": "p", "state": "blocked", "observed_at": "2026-10-01T00:10:00Z", "late": False},
        {"lsn": 3, "kind": "LandCheck", "pos": 1, "changeset": "p", "state": "bad", "observed_at": "2026-10-01T00:40:00Z", "late": False},
        {"lsn": 4, "kind": "LandReverted", "pos": 1, "changeset": "p", "revert": "r", "actor": "checker", "observed_at": "2026-10-01T00:50:00Z", "late": False},
        {"lsn": 5, "kind": "RefMoved", "pos": 2, "changeset": "q", "observed_at": "2026-10-01T01:00:00Z", "late": True},
        {"lsn": 6, "kind": "RefMoved", "pos": 3, "changeset": "s", "observed_at": "2026-10-02T00:00:00Z", "late": False},
        {"lsn": 7, "kind": "Attested", "pos": 3, "changeset": "s", "attest_kind": "review", "observed_at": "2026-10-02T01:00:00Z", "late": False},
    ]

    def test_m1_excludes_blocked_and_late(self):
        table = report.m1_table(self.ROWS)
        # 40 min observed, 30 of them blocked, so the sample is 10 min.
        self.assertEqual(table["p50_s"], 600)
        self.assertEqual(table["blocked_s"], 1800)
        self.assertEqual(table["late"], 1)
        self.assertEqual(table["pending"], 1)
        self.assertFalse(table["over_p95_bound"])
        self.assertEqual(table["over_max_bound"], 0)
        self.assertEqual(table["p95_bound_s"], 30 * 60)
        self.assertEqual(table["max_bound_s"], 4 * 3600)

    def test_over_bound_counts_the_max(self):
        rows = [
            {"lsn": 1, "kind": "RefMoved", "pos": 1, "changeset": "a", "observed_at": "2026-10-01T00:00:00Z", "late": False},
            {"lsn": 2, "kind": "LandCheck", "pos": 1, "changeset": "a", "state": "good", "observed_at": "2026-10-01T05:00:00Z", "late": False},
        ]
        table = report.m1_table(rows)
        self.assertEqual(table["over_max_bound"], 1)
        self.assertTrue(table["over_p95_bound"])

    def test_m2_and_m3_and_d31(self):
        plants = [{"changeset": "p", "variant": "fmt"}]
        batches = [{"pr": 9, "changesets": ["p", "r"]}]
        m2 = report.m2_table(self.ROWS, plants, batches)
        self.assertEqual(m2[0]["reverted_by"], "r")
        self.assertTrue(m2[0]["unattended"])
        self.assertEqual(m2[0]["batches"], [9])
        self.assertTrue(m2[0]["paired_with_revert"])
        m3 = report.m3_table(self.ROWS)
        self.assertEqual(m3[0], {"day": "2026-10-01", "lands": 1, "reviews": 0, "gap": 1})
        self.assertEqual(m3[1]["reviews"], 1)
        d31 = report.d31_table([1.0, 2.0, 3.0], [2.0])
        self.assertEqual(d31["before"]["n"], 3)
        self.assertIn('op="Land"', d31["query"])

    def test_render_is_recomputable(self):
        text = report.render(self.ROWS, [{"changeset": "p", "variant": "test"}], [{"pr": 9, "changesets": ["p", "r"]}], [1.0], [2.0])
        self.assertIn("| p95_s | 600.0 |", text)
        self.assertIn("| blocked_s | 1800.0 |", text)
        again = report.render(self.ROWS, [{"changeset": "p", "variant": "test"}], [{"pr": 9, "changesets": ["p", "r"]}], [1.0], [2.0])
        self.assertEqual(text, again)


class CliTest(unittest.TestCase):
    def test_scripts_run_isolated(self):
        tmp = Path(self.id().replace(".", "_"))
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        tmp.mkdir()
        out = tmp / "r.md"
        (tmp / "rows.jsonl").write_text("", encoding="utf-8")
        proc = subprocess.run(
            [sys.executable, "-I", str(ROOT / "postland" / "report.py"), "--jsonl", str(tmp / "rows.jsonl"), "--out", str(out)],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue(out.exists())


if __name__ == "__main__":
    unittest.main()
