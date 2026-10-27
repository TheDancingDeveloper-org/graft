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


FIXTURES = Path(__file__).resolve().parent / "fixtures"


class RecordTest(unittest.TestCase):
    def test_late_needs_a_journal_time_and_ignores_a_restart(self):
        # No `at`: the first entry after a restart is not late.
        fresh = record.observe([{"lsn": 260, "kind": "RefMoved", "body": json.dumps({"name": "backlog-1", "new": "c1"})}], now="2026-10-27T00:00:00Z", late_after_s=120)
        self.assertFalse(fresh[0]["late"])
        self.assertIsNone(fresh[0]["journaled_at"])
        # Late only when observation lags the journaled time by more than X.
        lagged = record.observe(
            [{"lsn": 1, "kind": "RefMoved", "body": json.dumps({"name": "backlog-1", "new": "c2", "at": "2026-10-26T00:00:00Z"})}],
            now="2026-10-27T00:00:00Z",
            late_after_s=120,
        )
        self.assertTrue(lagged[0]["late"])
        close = record.observe(
            [{"lsn": 2, "kind": "RefMoved", "body": json.dumps({"name": "backlog-1", "new": "c3", "at": "2026-10-27T00:00:00Z"})}],
            now="2026-10-27T00:01:00Z",
            late_after_s=120,
        )
        self.assertFalse(close[0]["late"])

    def test_resume_writes_no_duplicate(self):
        out = Path(self.id().replace(".", "_") + ".jsonl")
        self.addCleanup(out.unlink, missing_ok=True)
        seen: set[int] = set()
        rows = record.observe([{"lsn": 1, "kind": "RefMoved", "body": json.dumps({"name": "backlog-1", "new": "c1"})}], now="2026-10-27T00:00:00Z", late_after_s=120)
        self.assertEqual(len(record.append_rows(out, rows, seen)), 1)
        self.assertEqual(record.append_rows(out, rows, seen), [])

    def test_run_once_against_captured_fixtures(self):
        """Replay the graft-dev capture. A repo field or the old op fails the stub."""
        tmp = Path(self.id().replace(".", "_"))
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        tmp.mkdir()
        log = tmp / "calls"
        watch = FIXTURES / "watch-refmoved.json"
        checks = FIXTURES / "land-checks-empty.json"
        graft = tmp / "graft"
        graft.write_text(
            "#!" + sys.executable + "\n"
            "import pathlib, sys\n"
            "req = sys.stdin.read()\n"
            f"pathlib.Path({str(log)!r}).open('a').write(sys.argv[1] + ' ' + sys.argv[2] + chr(10) + req)\n"
            "needle = chr(34) + 'repo' + chr(34)\n"
            "if needle in req:\n"
            "    sys.stderr.write('unknown field repo\\n'); sys.exit(2)\n"
            "if sys.argv[2] != 'watch':\n"
            "    sys.exit(9)\n"
            f"sys.stdout.write(pathlib.Path({str(watch)!r}).read_text())\n",
            encoding="utf-8",
        )
        graft.chmod(0o755)
        http = tmp / "http"
        http.write_text(
            "#!" + sys.executable + "\n"
            "import pathlib, sys\n"
            "url = sys.argv[-1]\n"
            f"pathlib.Path({str(log)!r}).open('a').write(' '.join(sys.argv[1:]) + chr(10))\n"
            "if 'get_land_checks' in url:\n"
            "    sys.stderr.write('old op\\n'); sys.exit(9)\n"
            "if '/v1/repos/graft/land-checks' not in url:\n"
            "    sys.stderr.write('unexpected\\n'); sys.exit(9)\n"
            f"sys.stdout.write(pathlib.Path({str(checks)!r}).read_text())\n",
            encoding="utf-8",
        )
        http.chmod(0o755)
        token = tmp / "token"
        token.write_text("fixture-token\n", encoding="utf-8")
        out = tmp / "log.jsonl"
        state = tmp / "state.json"
        state.write_text(
            json.dumps({"lsn": 259, "seen": [], "pending": {"cpending": {"observed_at": "2026-10-27T00:00:00Z", "journaled_at": None}}}) + "\n",
            encoding="utf-8",
        )
        count = record.run_once(
            graft=str(graft), http=str(http), endpoint="http://graft-dev:17888",
            token_file=token, repo="graft", line="backlog-1", state_path=state, out_path=out,
            now="2026-10-27T03:00:00Z", page_limit=50, pending_limit=10, late_after_s=120,
        )
        self.assertGreater(count, 0)
        calls = log.read_text(encoding="utf-8")
        self.assertIn("api watch", calls)
        self.assertNotIn('"repo"', calls)
        self.assertIn("/v1/repos/graft/land-checks", calls)
        self.assertNotIn("get_land_checks", calls)
        self.assertIn("Authorization: Bearer", state.with_suffix(".header").read_text(encoding="utf-8"))
        rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
        self.assertTrue(rows)
        self.assertTrue(all(not row["late"] for row in rows))
        self.assertIn("cpending", json.loads(state.read_text(encoding="utf-8"))["pending"])

    def test_old_request_shape_is_rejected(self):
        """The reviewer's failure: posting repo inside the watch body."""
        tmp = Path(self.id().replace(".", "_"))
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        tmp.mkdir()
        stub = tmp / "graft"
        stub.write_text("#!/bin/sh\nreq=$(cat)\ncase \"$req\" in *'\"repo\"'*) echo \"unknown field 'repo'\" >&2; exit 2;; esac\nprintf '%s' '{\"entries\":[],\"next_lsn\":1}'\n", encoding="utf-8")
        stub.chmod(0o755)
        # The old recorder posted {"repo": ...}. Replaying that body must fail.
        proc = subprocess.run([str(stub), "api", "watch", "--input", "-"], input='{"repo":"graft","from_lsn":0}\n', text=True, capture_output=True)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("unknown field", proc.stderr)


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
        {"lsn": 1, "kind": "RefMoved", "changeset": "p", "journaled_at": "2026-10-01T00:00:00Z", "observed_at": "2026-10-01T00:01:00Z", "late": False},
        {"lsn": -1, "kind": "LandCheck", "changeset": "p", "state": "bad", "observed_at": "2026-10-01T00:11:00Z", "late": False},
        {"lsn": 4, "kind": "LandReverted", "changeset": "p", "revert": "r", "actor": "checker", "observed_at": "2026-10-01T00:50:00Z", "late": False},
        {"lsn": 5, "kind": "RefMoved", "changeset": "q", "journaled_at": None, "observed_at": "2026-10-01T01:00:00Z", "late": True},
        {"lsn": 6, "kind": "RefMoved", "changeset": "s", "journaled_at": None, "observed_at": "2026-10-02T00:00:00Z", "late": False},
        {"lsn": 7, "kind": "Attested", "changeset": "s", "attest_kind": "review", "observed_at": "2026-10-02T01:00:00Z", "late": False},
    ]

    def test_m1_uses_journal_time_and_does_not_invent_blocked(self):
        table = report.m1_table(self.ROWS, interval_s=60)
        # Landed at the journal time, settled at first observation: 11 min.
        self.assertEqual(table["p50_s"], 660)
        self.assertEqual(table["blocked"], report.BLOCKED_NA)
        self.assertNotEqual(table["blocked"], 0)
        self.assertEqual(table["late"], 1)
        self.assertEqual(table["pending"], 1)
        self.assertIn("60s", table["precision"])
        self.assertFalse(table["over_p95_bound"])

    def test_blocked_counts_only_a_journaled_attester_report(self):
        rows = self.ROWS + [{"lsn": 9, "kind": "AttesterBlocked", "changeset": "p", "observed_at": "2026-10-01T00:05:00Z", "late": False}]
        table = report.m1_table(rows)
        self.assertEqual(table["blocked"], 1.0)

    def test_over_bound_counts_the_max(self):
        rows = [
            {"lsn": 1, "kind": "RefMoved", "changeset": "a", "journaled_at": "2026-10-01T00:00:00Z", "observed_at": "2026-10-01T00:00:00Z", "late": False},
            {"lsn": 2, "kind": "LandCheck", "changeset": "a", "state": "good", "observed_at": "2026-10-01T05:00:00Z", "late": False},
        ]
        table = report.m1_table(rows)
        self.assertEqual(table["over_max_bound"], 1)
        self.assertTrue(table["over_p95_bound"])

    def test_m2_m3_and_d31_from_prometheus(self):
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
        before = {"status": "success", "data": {"resultType": "vector", "result": [{"value": [1, "1.5"]}]}}
        after = {"status": "success", "data": {"resultType": "vector", "result": [{"value": [2, "0.4"]}]}}
        d31 = report.d31_table(before, after)
        self.assertEqual(d31["before"], 1.5)
        self.assertEqual(d31["after"], 0.4)
        self.assertIn("histogram_quantile", d31["query"])
        self.assertEqual(d31["source"], "prometheus http api")

    def test_render_is_recomputable(self):
        empty = {"data": {"result": []}}
        text = report.render(self.ROWS, [{"changeset": "p", "variant": "test"}], [{"pr": 9, "changesets": ["p", "r"]}], empty, empty, interval_s=60)
        self.assertIn("| p95_s | 660.0 |", text)
        self.assertIn("| blocked | n/a (not observable) |", text)
        again = report.render(self.ROWS, [{"changeset": "p", "variant": "test"}], [{"pr": 9, "changesets": ["p", "r"]}], empty, empty, interval_s=60)
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
