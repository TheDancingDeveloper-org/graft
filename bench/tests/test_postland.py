"""Recorder, planter and report against captured graft-dev pages.

Watch pages ``watch-1100.json``, ``watch-1280.json`` and ``watch-1328.json``
and ``land-checks-empty.json`` are captured from graft-dev (line
``backlog-1``). graft-dev has no post-land line, so the land-checks pages
with rows and the post-land watch page are built in the server's shapes;
each file's ``_source`` says from where. No test sleeps or opens a socket.
"""

from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from postland import plant, record, report  # noqa: E402

FIXTURES = Path(__file__).resolve().parent / "fixtures"
TOKEN = "fixture-token-not-real"
CDA = "cda2b50efec7d12bc89f990cc98562a4c44bce2d80c3594b5a33835d2563e7122"
C69 = "c697026aa21db3908d406629f07592edab08b559b1b44a66cd31d4314e3142291"
CC5 = "cc5921bbbe7e738fb0d54cf02ea34b6ebe3ae65b5f9453dae72bfd3800175a1cd"
CFB = "cfb6ff9da71fdf5925d821357768292a8ee6265db0e9ec0cad7d7e9e1a93ad91f"
C40 = "c40208d1dacdfcb31e8bc1f7db54f2936bb1a12b754614a79d632b4bb8d4f6c46"
REVERT = "c13ebd5854498b57c7e5857544a73765ba21a3b0e60b675ff8b18820c69d364b8"
T0, T1, T2, T3 = "2026-10-27T07:00:00Z", "2026-10-27T07:01:00Z", "2026-10-27T07:02:00Z", "2026-10-27T07:20:00Z"

# Fields of the CLI's `api` input (crates/graft-cli/src/api.rs `Input`,
# deny_unknown_fields). The stub refuses any other field, as the CLI does.
CLI_INPUT_FIELDS = {"name", "ids", "at", "path", "from", "to", "a", "b", "until", "paths", "prefix", "recursive", "patch", "lines", "from_lsn", "kinds", "wait_ms", "limit", "cursor"}

GRAFT_STUB = """\
import json, os, pathlib, sys
argv = sys.argv[1:]
req = json.loads(sys.stdin.read())
log = pathlib.Path(os.environ["STUB_LOG"])
with log.open("a") as fh:
    fh.write(json.dumps({"argv": argv, "req": req}) + "\\n")
if argv[:2] != ["api", "watch"] or "--repo" not in argv or "--token-file" not in argv:
    sys.stderr.write("bad argv\\n"); sys.exit(2)
unknown = set(req) - set(json.loads(os.environ["STUB_FIELDS"]))
if unknown:
    sys.stderr.write(f"bad request: unknown field {sorted(unknown)[0]!r}\\n"); sys.exit(2)
pages = json.loads(pathlib.Path(os.environ["STUB_PAGES"]).read_text())
name = pages.get(str(req["from_lsn"]))
if name is None:
    page = {"entries": [], "next_lsn": req["from_lsn"]}
else:
    page = json.loads(pathlib.Path(os.environ["STUB_FIXTURES"], name).read_text())
    page.pop("_source", None)
sys.stdout.write(json.dumps(page))
"""


class Server:
    """Land-checks pages served through ``record.fetch_json``."""

    def __init__(self):
        self.pages: dict = {}
        self.calls: list[tuple[int, str | None]] = []

    def fetch_json(self, url, headers):
        parts = urlsplit(url)
        assert parts.path == "/v1/repos/graft/land-checks", parts.path
        assert headers == {"Authorization": f"Bearer {TOKEN}"}, "token must ride the Authorization header"
        assert TOKEN not in url
        query = {k: v[0] for k, v in parse_qs(parts.query).items()}
        assert query["line"] == "backlog-1"
        key = query["from_pos"] + ("/" + query["page_token"] if "page_token" in query else "")
        self.calls.append((int(query["from_pos"]), query.get("page_token")))
        return self.pages[key]


class Recorder(unittest.TestCase):
    """Drives ``record.run_once`` through the captured polls."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.stub = self.tmp / "graft"
        self.stub.write_text("#!" + sys.executable + "\n" + GRAFT_STUB, encoding="utf-8")
        self.stub.chmod(0o755)
        self.token = self.tmp / "token"
        self.token.write_text(TOKEN + "\n", encoding="utf-8")
        self.token.chmod(0o600)
        self.state = self.tmp / "state.json"
        self.out = self.tmp / "log.jsonl"
        self.log = self.tmp / "calls.jsonl"
        self.watch_pages = self.tmp / "pages.json"
        self.server = Server()
        original = record.fetch_json
        record.fetch_json = self.server.fetch_json
        self.addCleanup(setattr, record, "fetch_json", original)
        env = {"STUB_LOG": str(self.log), "STUB_PAGES": str(self.watch_pages), "STUB_FIXTURES": str(FIXTURES), "STUB_FIELDS": json.dumps(sorted(CLI_INPUT_FIELDS))}
        for key, value in env.items():
            self.addCleanup(os.environ.pop, key, None)
            os.environ[key] = value
        self.checks = json.loads((FIXTURES / "land-checks-pages.json").read_text(encoding="utf-8"))

    def poll(self, now, watch, checks, **bootstrap):
        self.watch_pages.write_text(json.dumps(watch), encoding="utf-8")
        self.server.pages = checks
        self.server.calls = []
        return record.run_once(
            graft=str(self.stub), endpoint="http://graft-dev:17888", token_file=self.token, repo="graft", line="backlog-1",
            state_path=self.state, out_path=self.out, now=now, watch_limit=500, checks_limit=2, late_after_s=120, **bootstrap,
        )

    def rows(self):
        return [json.loads(line) for line in self.out.read_text(encoding="utf-8").splitlines()]

    def run_all(self):
        """Four polls: bootstrap, lands, settlements and a revert, then a gap."""
        empty = json.loads((FIXTURES / "land-checks-empty.json").read_text(encoding="utf-8"))
        self.poll(T0, {"1100": "watch-1100.json"}, {"84": empty}, from_lsn=1100, from_pos=84)
        self.poll(T1, {"1280": "watch-1280.json", "1328": "watch-1328.json"}, self.checks["poll1"])
        self.poll(T2, {"1328": "watch-1328-postland.json"}, self.checks["poll2"])
        self.poll(T3, {}, self.checks["poll3"])
        return self.rows()


class RecordTest(Recorder):
    def test_watch_request_is_the_cli_shape(self):
        self.poll(T0, {"1100": "watch-1100.json"}, {"84": {"rows": [], "next": None}}, from_lsn=1100, from_pos=84)
        calls = [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()]
        # Page until next_lsn stops moving: 1100 -> 1280, then 1280 is the head.
        self.assertEqual([c["req"]["from_lsn"] for c in calls], [1100, 1280])
        for call in calls:
            self.assertEqual(call["argv"][:5], ["api", "watch", "--repo", "graft", "--token-file"])
            self.assertNotIn(TOKEN, " ".join(call["argv"]))
            self.assertEqual(call["req"]["kinds"], ["RefMoved", "Attested", "LandReverted"])

    def test_watch_with_an_unknown_field_fails(self):
        """`record.watch` against the strict stub: a body field it does not know is refused."""
        os.environ["STUB_FIELDS"] = json.dumps(sorted(CLI_INPUT_FIELDS - {"wait_ms"}))
        self.watch_pages.write_text("{}", encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "unknown field 'wait_ms'"):
            record.watch(str(self.stub), "graft", self.token, 1100, 10)

    def test_real_journal_rows_keep_only_the_line(self):
        self.poll(T0, {"1100": "watch-1100.json"}, {"84": {"rows": [], "next": None}}, from_lsn=1100, from_pos=84)
        rows = self.rows()
        moved = [r for r in rows if r["kind"] == "RefMoved"]
        # watch-1100.json holds 14 backlog-1 moves and 11 main moves.
        self.assertEqual(len(moved), 14)
        self.assertTrue(all(r["observed_at"] == T0 for r in rows))
        attested = [r for r in rows if r["kind"] == "Attested"]
        self.assertEqual(len(attested), 1)
        # A v0.1 body: no kind, `passed: true`.
        self.assertEqual((attested[0]["attest_kind"], attested[0]["status"]), ("check", "pass"))
        # The server sends no time and no position; the row invents neither.
        self.assertTrue(all("journaled_at" not in r and "pos" not in r for r in moved))

    def test_first_poll_is_late_and_a_gap_is_flagged(self):
        rows = self.run_all()
        self.assertTrue(all(r["late"] for r in rows if r.get("observed_at") == T0))
        self.assertFalse(any(r["late"] for r in rows if r.get("observed_at") in (T1, T2)))
        gaps = [r for r in rows if r["kind"] == "Gap"]
        self.assertEqual(gaps, [{"kind": "Gap", "from": T2, "to": T3, "seconds": 1080.0}])
        self.assertTrue(all(r["late"] for r in rows if r.get("observed_at") == T3))

    def test_land_pending_in_one_poll_settles_in_the_next(self):
        rows = self.run_all()
        checks = [(r["observed_at"], r["changeset"], r["state"]) for r in rows if r["kind"] == "LandCheck"]
        self.assertIn((T1, C69, "pending"), checks)
        self.assertIn((T2, C69, "good"), checks)
        self.assertIn((T1, CC5, "bad"), checks)
        self.assertIn((T2, CC5, "reverted"), checks)
        self.assertIn((T2, REVERT, "good"), checks)
        self.assertIn((T3, CFB, "good"), checks)
        # An unchanged state is not logged again, and nothing is logged twice.
        self.assertEqual(len(checks), len(set(checks)))
        self.assertNotIn((T2, CFB, "pending"), checks)
        state = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertEqual(state["pending"], {})
        self.assertEqual(state["frontier"], 90)
        self.assertEqual(state["lsn"], 1337)

    def test_checks_start_at_the_oldest_pending_position(self):
        self.run_all()
        # Poll 3 starts at cfb6ff's position (87), the only land still pending.
        self.assertEqual(self.server.calls, [(87, None), (87, "89")])

    def test_checks_follow_next_and_pending_is_kept_in_state(self):
        empty = json.loads((FIXTURES / "land-checks-empty.json").read_text(encoding="utf-8"))
        self.poll(T0, {"1100": "watch-1100.json"}, {"84": empty}, from_lsn=1100, from_pos=84)
        self.assertEqual(self.server.calls, [(84, None)])
        self.poll(T1, {"1280": "watch-1280.json"}, self.checks["poll1"])
        self.assertEqual(self.server.calls, [(84, None), (84, "86"), (84, "88")])
        state = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertEqual(state["pending"], {C69: {"pos": 85, "state": "pending"}, CC5: {"pos": 86, "state": "bad"}, CFB: {"pos": 87, "state": "pending"}, C40: {"pos": 88, "state": "pending"}})
        self.poll(T2, {"1328": "watch-1328-postland.json"}, self.checks["poll2"])
        self.assertEqual(self.server.calls[0], (85, None))

    def test_restart_after_a_torn_poll_writes_no_duplicate(self):
        empty = json.loads((FIXTURES / "land-checks-empty.json").read_text(encoding="utf-8"))
        self.poll(T0, {"1100": "watch-1100.json"}, {"84": empty}, from_lsn=1100, from_pos=84)
        committed = self.state.read_text(encoding="utf-8")
        self.poll(T1, {"1280": "watch-1280.json"}, self.checks["poll1"])
        # The poll's rows reached the log but its state write did not.
        self.state.write_text(committed, encoding="utf-8")
        self.poll(T2, {"1280": "watch-1280.json"}, self.checks["poll1"])
        rows = self.rows()
        keys = [(r["kind"], r.get("lsn"), r.get("changeset"), r.get("state")) for r in rows]
        self.assertEqual(len(keys), len(set(keys)))
        self.assertFalse(any(r.get("observed_at") == T1 for r in rows))
        self.assertEqual(sum(1 for r in rows if r.get("lsn") == 1324), 1)

    def test_apply_checks(self):
        state = {"frontier": 86, "pending": {C69: {"pos": 85, "state": "pending"}}}
        rows = [
            {"pos": 84, "changeset": CDA, "state": "good", "revert": None},
            {"pos": 85, "changeset": C69, "state": "pending", "revert": None},
            {"pos": 86, "changeset": CC5, "state": "bad", "revert": None},
        ]
        out = record.apply_checks(state, rows)
        # 84 is below the frontier and not followed; 85 did not move; 86 is new.
        self.assertEqual([(r["pos"], r["state"]) for r in out], [(86, "bad")])
        self.assertEqual(state["frontier"], 87)
        out = record.apply_checks(state, [{"pos": 86, "changeset": CC5, "state": "reverted", "revert": REVERT}])
        self.assertEqual([(r["changeset"], r["state"], r["revert"]) for r in out], [(CC5, "reverted", REVERT)])
        self.assertEqual(state["pending"], {C69: {"pos": 85, "state": "pending"}})
        self.assertEqual(record.checks_start(state), 85)

    def test_token_never_reaches_disk_or_argv(self):
        self.run_all()
        for path in self.tmp.rglob("*"):
            if path.is_file() and path != self.token:
                self.assertNotIn(TOKEN, path.read_text(encoding="utf-8"), path.name)
        self.assertEqual(stat.S_IMODE(self.token.stat().st_mode), 0o600)


class ReportTest(Recorder):
    def test_m1_from_the_recorded_polls(self):
        rows = self.run_all()
        table = report.m1_rows(rows, None)
        day = next(r for r in table if r["day"] == "all")
        # Checked lands: cda2b5, c69702, cc5921, cfb6ff, c40208 and the revert.
        self.assertEqual(day["lands"], 6)
        self.assertEqual(day["unchecked"], 14)
        # Samples: cda 0, c69 60, cc5 0 (bad), c40 60, revert 0; cfb6ff settled after the gap.
        self.assertEqual(day["samples"], 5)
        self.assertEqual((day["p50_s"], day["p95_s"], day["max_s"]), (0.0, 60.0, 60.0))
        self.assertEqual((day["late"], day["pending"]), (1, 0))
        self.assertTrue(day["within_bound"])
        self.assertEqual(day["blocked_s"], report.NOT_RECORDED)
        self.assertEqual(report.bad_to_reverted(rows), {"count": 1, "p50_s": 60.0, "max_s": 60.0})

    def test_blocked_windows_are_cut_and_reported(self):
        rows = self.run_all()
        held = [(report.parse_time("2026-10-27T07:01:30Z"), report.parse_time("2026-10-27T07:01:50Z"))]
        day = report.m1_rows(rows, held)[-1]
        self.assertEqual(day["blocked_s"], 40.0)
        self.assertEqual(day["max_s"], 40.0)

    def test_pending_at_the_end_of_the_day(self):
        rows = self.run_all()
        rows = [dict(r, observed_at="2026-10-28T00:10:00Z") if r.get("changeset") == C69 and r.get("state") == "good" else r for r in rows]
        table = report.m1_rows(rows, None)
        self.assertEqual(table[0]["day"], "2026-10-27")
        self.assertEqual(table[0]["pending"], 1)
        self.assertEqual(table[-1]["pending"], 0)

    def test_over_bound(self):
        rows = [
            {"kind": "RefMoved", "changeset": "a", "observed_at": "2026-10-01T00:00:00Z", "late": False},
            {"kind": "LandCheck", "changeset": "a", "state": "good", "observed_at": "2026-10-01T05:00:00Z", "late": False},
        ]
        day = report.m1_rows(rows, None)[-1]
        self.assertEqual((day["over_30m"], day["over_4h"], day["within_bound"]), (1, 1, False))

    def test_m2_and_m3(self):
        rows = self.run_all()
        plants = [{"plant": CC5, "follower": CFB, "variant": "test"}]
        m2 = report.m2_rows(rows, plants, [{"pr": 300, "changesets": [CC5, REVERT]}], "graft-checker")
        self.assertEqual(m2[0]["reverted_by"], REVERT)
        self.assertEqual(m2[0]["revert_actor"], "graft-checker")
        self.assertEqual((m2[0]["plant_state"], m2[0]["revert_state"]), ("reverted", "good"))
        self.assertTrue(m2[0]["unattended"])
        self.assertTrue(m2[0]["never_alone"])
        alone = report.m2_rows(rows, plants, [{"pr": 301, "changesets": [CC5]}], "someone-else")
        self.assertFalse(alone[0]["never_alone"])
        self.assertFalse(alone[0]["unattended"])
        m3 = report.m3_rows(rows)
        self.assertEqual(m3, [{"day": "2026-10-27", "lands": 20, "reviewed": 1, "unreviewed": 19}])


class D31Test(unittest.TestCase):
    def test_before_and_after_are_queried_at_their_own_instants(self):
        seen = []

        def fake(url):
            query = {k: v[0] for k, v in parse_qs(urlsplit(url).query).items()}
            seen.append((query["query"], query["time"]))
            if query["query"].startswith("avg_over_time(up"):
                return {"status": "success", "data": {"result": [{"value": [0, "1"]}]}}
            before = query["time"] <= "2026-10-10T00:00:00Z"
            value = {("0.5", True): "0.2", ("0.95", True): "0.5", ("0.5", False): "0.25", ("0.95", False): "0.55"}[(query["query"][19:23].rstrip(","), before)]
            return {"status": "success", "data": {"result": [{"value": [0, value]}]}}

        original = report.fetch_json
        report.fetch_json = fake
        self.addCleanup(setattr, report, "fetch_json", original)
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (tmp / "log.jsonl").write_text("", encoding="utf-8")
        report.main(["--jsonl", str(tmp / "log.jsonl"), "--out", str(tmp / "r.md"), "--prometheus", "http://prom:9090", "--enabled-at", "2026-10-10T00:00:00Z", "--days", "2", "--now", "2026-10-11T12:00:00Z"])
        times = sorted({t for _, t in seen})
        # Two days before (ending 10-09 and 10-10), one after (10-11); 10-12 is in the future.
        self.assertEqual(times, ["2026-10-09T00:00:00Z", "2026-10-10T00:00:00Z", "2026-10-11T00:00:00Z"])
        self.assertIn('histogram_quantile(0.95, sum by (le) (rate(graft_rpc_duration_seconds_bucket{job="graft-dev",op="Land"}[1d])))', {q for q, _ in seen})
        text = (tmp / "r.md").read_text(encoding="utf-8")
        self.assertIn("| before | 2026-10-09T00:00:00Z | 0.2 | 0.5 | 1 |", text)
        self.assertIn("| after | 2026-10-11T00:00:00Z | 0.25 | 0.55 | 1 |", text)
        # Ceiling 0.5 x 1.2 = 0.6; 0.55 holds.
        self.assertIn("(ceiling 0.6s, 2 before and 1 after days): True.", text)

    def test_verdict_fails_over_the_ceiling(self):
        table = [{"side": "before", "p95_s": 1.0}, {"side": "before", "p95_s": 2.0}, {"side": "after", "p95_s": 2.5}]
        self.assertFalse(report.d31_verdict(table)["holds"])


class PlantTest(unittest.TestCase):
    def test_fmt_and_test_land_two_and_print_both_ids(self):
        for variant, needle in (("fmt", "let value=1;"), ("test", "panic!()")):
            tmp = Path(tempfile.mkdtemp())
            self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
            calls = tmp / "calls"
            stub = tmp / "graft"
            # `graft land --json` answers {"outcome": "landed", "landed": id}
            # (crates/graft-cli/src/cmd/land.rs); the first land is contended once.
            stub.write_text(
                "#!" + sys.executable + "\n"
                "import json, os, pathlib, sys\n"
                f"calls = pathlib.Path({str(calls)!r})\n"
                "with calls.open('a') as fh: fh.write(json.dumps([sys.argv[1:], os.environ['GRAFT_SESSION_ID'], os.environ['GRAFT_WORK_ITEM']]) + chr(10))\n"
                "n = len(calls.read_text().splitlines())\n"
                "if sys.argv[1] == 'land':\n"
                "    if n == 2: sys.exit(4)\n"
                "    print(json.dumps({'outcome': 'landed', 'line': 'exit-rehearsal', 'landed': 'c%064d' % n}))\n",
                encoding="utf-8",
            )
            stub.chmod(0o755)
            out = subprocess.run(
                [sys.executable, "-I", str(ROOT / "postland" / "plant.py"), "--variant", variant, "--root", str(tmp), "--graft", str(stub), "--record", str(tmp / "plants.jsonl")],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(out.returncode, 0, out.stderr)
            plant_id, follower_id = out.stdout.split()
            self.assertEqual((plant_id, follower_id), ("c%064d" % 3, "c%064d" % 5))
            body = (tmp / plant.PLANTED).read_text(encoding="utf-8")
            self.assertIn(needle, body)
            self.assertTrue(body.endswith(plant.FOLLOWER))
            log = [json.loads(line) for line in calls.read_text(encoding="utf-8").splitlines()]
            self.assertEqual([c[0][0] for c in log], ["commit", "land", "land", "commit", "land"])
            self.assertTrue(all(c[1:] == ["postland-plant", "WI-2604"] for c in log))
            recorded = json.loads((tmp / "plants.jsonl").read_text(encoding="utf-8"))
            self.assertEqual((recorded["plant"], recorded["follower"], recorded["variant"]), (plant_id, follower_id, variant))


class CliTest(unittest.TestCase):
    def test_report_runs_isolated(self):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (tmp / "rows.jsonl").write_text("", encoding="utf-8")
        proc = subprocess.run(
            [sys.executable, "-I", str(ROOT / "postland" / "report.py"), "--jsonl", str(tmp / "rows.jsonl"), "--out", str(tmp / "r.md")],
            check=False, capture_output=True, text=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("Blocked time: not recorded", (tmp / "r.md").read_text(encoding="utf-8"))

    def test_record_needs_a_start_on_the_first_poll(self):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        proc = subprocess.run(
            [sys.executable, "-I", str(ROOT / "postland" / "record.py"), "--repo", "graft", "--line", "backlog-1", "--state", str(tmp / "s.json"), "--out", str(tmp / "o.jsonl"), "--token-file", str(tmp / "t")],
            check=False, capture_output=True, text=True,
        )
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("--from-lsn and --from-pos", proc.stderr)


if __name__ == "__main__":
    unittest.main()
