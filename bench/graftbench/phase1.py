"""Phase-1 features of WI-2349 (design §11.4 step 8a): the pre-land check
in `graft land` (D27), one commit per land in batch PRs (D26) and identity
binding (step 6). No build, test or check runs anywhere (D22).

    python3 -m graftbench.phase1 --bin-dir ../target/release --out results/<dir>

It starts its own graft-server (a temp dir, loopback), a bare git upstream
and a fake GitHub API on loopback (pull requests only), so nothing reaches
GitHub. With --server and --admin-token-file it runs against a running
server instead; the repositories it names must exist there.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from .proc import StepError, run, summarize
from .site import LocalSite
from .tools import Graft, Mirror

LINE = "main"  # the line the identity and pre-land cases land on (--line)
SESSION = "ses_01BENCH8A000000000000000000"
WORK_ITEM = "WI-2349"
OWNER, GHREPO = "bench", "upstream"


# ---------------------------------------------------------------- fake GitHub


class FakeGithub:
    """The pull-request endpoints pr-batch calls, answered from a bare repo."""

    def __init__(self, bare: Path):
        self.bare = bare
        self.pulls: dict[int, dict] = {}
        self.lock = threading.Lock()
        fake = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def _send(self, code, body):
                data = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def _body(self):
                n = int(self.headers.get("content-length") or 0)
                return json.loads(self.rfile.read(n) or b"{}")

            def do_GET(self):
                u = urllib.parse.urlparse(self.path)
                q = urllib.parse.parse_qs(u.query)
                parts = u.path.strip("/").split("/")
                if parts[-1] == "pulls":
                    head = q.get("head", [""])[0].split(":", 1)[-1]
                    with fake.lock:
                        out = [fake.view(p) for p in fake.pulls.values() if not head or p["head"] == head]
                    return self._send(200, out)
                if len(parts) >= 2 and parts[-2] == "pulls":
                    with fake.lock:
                        p = fake.pulls.get(int(parts[-1]))
                    return self._send(200, fake.view(p)) if p else self._send(404, {"message": "Not Found"})
                if parts[-1] in ("reviews", "comments"):
                    return self._send(200, [])
                if parts[-1] == "check-runs":
                    return self._send(200, {"total_count": 0, "check_runs": []})
                return self._send(404, {"message": "Not Found"})

            def do_POST(self):
                body = self._body()
                parts = self.path.split("?")[0].strip("/").split("/")
                if parts[-1] == "pulls":
                    with fake.lock:
                        n = len(fake.pulls) + 1
                        fake.pulls[n] = {"number": n, "state": "open", **body}
                        return self._send(201, fake.view(fake.pulls[n]))
                if parts[-1] == "graphql":
                    return self._send(200, {"data": {}})
                return self._send(201, {})

            def do_PATCH(self):
                body = self._body()
                n = int(self.path.split("?")[0].rstrip("/").split("/")[-1])
                with fake.lock:
                    fake.pulls[n].update(body)
                    return self._send(200, fake.view(fake.pulls[n]))

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def view(self, p: dict) -> dict:
        sha = run(["git", "rev-parse", f"refs/heads/{p['head']}"], cwd=self.bare, check=False).out.strip()
        return {
            "number": p["number"],
            "html_url": f"{self.url}/pull/{p['number']}",
            "node_id": f"PR_{p['number']}",
            "state": p["state"],
            "merged": False,
            "merge_commit_sha": None,
            "head": {"sha": sha or "0" * 40},
        }

    def stop(self):
        self.httpd.shutdown()


class DeployedSite:
    """A running graft-server container: admin commands by `docker exec`,
    tokens straight from its stdout into 0600 files, never printed."""

    def __init__(self, container: str, url: str, dir: Path):
        self.container = container
        self.url = url.rstrip("/")
        dir.mkdir(parents=True, exist_ok=True)
        self.token_file = dir / "admin.token"
        self._admin_repo = None

    def admin(self, args: list):
        r = run(["docker", "exec", "-i", self.container, "graft-server", "admin"] + args, check=False)
        if args[:2] == ["repo", "create"] and r.rc != 0 and "exists" not in (r.err + r.out):
            raise StepError(f"repo create failed: {r.err[-500:]}")
        if args[:2] == ["repo", "create"] and self._admin_repo is None:
            self._admin_repo = args[2]
            scopes = [f"repo:{args[2]}:{s}" for s in ("read", "write", "admin", "land:main", f"land:{LINE}")]
            m = ["token", "mint", "--subject", "graft-bench-8a", "--kind", "service", "--ttl", "86400"]
            for sc in scopes:
                m += ["--scope", sc]
            write_token(self.token_file, self.admin(m).out)
        if r.rc != 0 and args[:2] != ["repo", "create"]:
            raise StepError(f"admin {args[:2]} failed: {r.err[-500:]}")
        return r

    def stats(self) -> dict:
        return {}

    def stop(self) -> None:
        pass


# ---------------------------------------------------------------- helpers


def write_token(path: Path, text: str) -> Path:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(text.strip() + "\n")
    return path


def mint(site: LocalSite, out: Path, subject: str, kind: str, scopes: list[str], extra: list[str] = ()) -> Path:
    args = ["token", "mint", "--subject", subject, "--kind", kind, "--ttl", "86400", *extra]
    for s in scopes:
        args += ["--scope", s]
    return write_token(out, site.admin(args).out)


def git(args, cwd, env=None):
    base = {
        "GIT_AUTHOR_NAME": "upstream",
        "GIT_AUTHOR_EMAIL": "upstream@example.invalid",
        "GIT_COMMITTER_NAME": "upstream",
        "GIT_COMMITTER_EMAIL": "upstream@example.invalid",
    }
    base.update(env or {})
    return run(["git"] + list(args), cwd=cwd, env=base)


def project_files(n: int = 40) -> dict[str, str]:
    files = {f"src/mod_{i:03}.py": "".join(f"def f{i}_{j}():\n    return {j}\n\n" for j in range(20)) for i in range(n)}
    files["CHANGELOG.md"] = "# Changelog\n\n" + "".join(f"- entry {j}\n" for j in range(20))
    return files


def write_files(root: Path, files: dict[str, str]) -> None:
    for rel, text in files.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)


def edit_line(path: Path, idx: int, new: str) -> None:
    lines = path.read_text().splitlines(keepends=True)
    lines[idx] = new + "\n"
    path.write_text("".join(lines))


# ---------------------------------------------------------------- pre-land check


def bench_preland(site, g_bin: Path, work: Path, repo: str, token: Path, k: int) -> dict:
    """Land latency with the pre-land check (default) against --no-check, in
    three cases: up to date, one land behind on another file, and a real
    conflict on the same line."""
    g = Graft(g_bin, site.url, token)
    a, b = work / "pl-a", work / "pl-b"
    g(["clone", repo, a, "--line", LINE])
    g(["clone", repo, b, "--line", LINE])
    samples: dict[str, list[float]] = {}
    stops = {"check": 0, "nocheck_refused": 0, "sent_by_check": 0}

    def land(wc, check: bool):
        args = ["land"] + ([] if check else ["--no-check"])
        return g.json(args, cwd=wc, check=False)

    for i in range(k):
        for check in (True, False):
            mode = "check" if check else "no_check"
            # up to date
            g(["sync"], cwd=a)
            edit_line(a / "src/mod_000.py", 1, f"    return {i}{int(check)}0")
            g(["commit", "-m", f"fresh {i} {mode}"], cwd=a)
            r, _ = land(a, check)
            if r.rc != 0:
                raise StepError(f"fresh land failed: {r.err[-500:]}")
            samples.setdefault(f"fresh_{mode}", []).append(r.secs)
            # one behind: b lands another file first, then a lands
            g(["sync"], cwd=b)
            edit_line(b / "src/mod_001.py", 1, f"    return {i}{int(check)}1")
            g(["commit", "-m", f"other {i} {mode}"], cwd=b)
            g(["land"], cwd=b)
            edit_line(a / "src/mod_002.py", 1, f"    return {i}{int(check)}2")
            g(["commit", "-m", f"behind {i} {mode}"], cwd=a)
            r, _ = land(a, check)
            if r.rc != 0:
                raise StepError(f"behind land failed: {r.err[-500:]}")
            samples.setdefault(f"behind_{mode}", []).append(r.secs)
    # conflicts: a fresh clone per case, b lands the same line first
    for i in range(k):
        for check in (True, False):
            mode = "check" if check else "no_check"
            c = work / f"pl-c-{i}-{mode}"
            g(["clone", repo, c, "--line", LINE])
            g(["sync"], cwd=b)
            edit_line(b / "CHANGELOG.md", 3, f"- b {i} {mode}")
            g(["commit", "-m", f"b conflict {i}"], cwd=b)
            g(["land"], cwd=b)
            tip_before = g.json(["log", "--limit", "1"], cwd=b)[1]["changesets"][0]["id"]
            edit_line(c / "CHANGELOG.md", 3, f"- c {i} {mode}")
            g(["commit", "-m", f"c conflict {i}"], cwd=c)
            r, j = land(c, check)
            tip_after = g.json(["log", "--limit", "1"], cwd=b)[1]["changesets"][0]["id"]
            samples.setdefault(f"conflict_{mode}", []).append(r.secs)
            if r.rc == 0 or tip_after != tip_before:
                key = "sent_by_check" if check else "landed_conflict"
                stops[key] = stops.get(key, 0) + 1
            elif check:
                stops["check"] += 1
            else:
                stops["nocheck_refused"] += 1
            shutil.rmtree(c)
    return {
        "k": k,
        "latency": {name: summarize(s) for name, s in samples.items()},
        "conflicts": {"cases": k, **stops},
    }


# ---------------------------------------------------------------- batch export


def bench_batch(site, bins: Path, work: Path, n: int, admin_scopes_for, prefix: str = "p1") -> dict:
    """N lands on an integration line, then one pr-batch run into a local
    upstream through the fake GitHub: commits on the PR branch, trailers,
    squash fallback, and time."""
    repo = f"{prefix}b-{n}"
    site.admin(["repo", "create", repo])
    d = work / f"batch-{n}"
    d.mkdir()
    up = d / "up.git"
    run(["git", "init", "-q", "--bare", "-b", "main", up])
    seed = d / "seed"
    run(["git", "init", "-q", "-b", "main", seed])
    write_files(seed, project_files())
    git(["add", "-A"], seed)
    git(["commit", "-qm", "seed"], seed)
    git(["push", "-q", str(up), "main"], seed)

    mtoken = mint(site, d / "mirror.token", "bench-mirror", "service", admin_scopes_for(repo))
    atoken = mint(
        site,
        d / "agent.token",
        "bench-agent",
        "agent",
        [f"repo:{repo}:read", f"repo:{repo}:write", f"repo:{repo}:land:batch"],
        ["--session", SESSION, "--work-item", WORK_ITEM],
    )
    ghtok = write_token(d / "gh.token", "fake-github-token")
    fake = FakeGithub(up)
    cfg = d / "mirror.toml"
    cfg.write_text(
        f'server = "{site.url}"\nrepo = "{repo}"\nupstream = "{up}"\n'
        f'mirror_dir = "{d / "mirror.git"}"\ndb = "{d / "mirror.sqlite"}"\n'
        f'token_file = "{mtoken}"\npoll_interval_secs = 1\njson_logs = false\n'
        f'\n[[lines]]\nbranch = "main"\nline = "main"\n'
        f'\n[github]\napi = "{fake.url}"\nowner = "{OWNER}"\nrepo = "{GHREPO}"\ntoken_file = "{ghtok}"\n'
    )
    mirror = bins / "graft-mirror"
    gm = Graft(bins / "graft", site.url, mtoken)
    ga = Graft(bins / "graft", site.url, atoken)

    def follow_until(pred, what):
        log = open(d / "follow.log", "ab")
        p = subprocess.Popen([str(mirror), "follow", "--config", str(cfg)], stdout=log, stderr=log, stdin=subprocess.DEVNULL)
        try:
            for _ in range(120):
                if pred():
                    return
                time.sleep(0.5)
            raise StepError(f"follow did not {what}; see {d / 'follow.log'}")
        finally:
            p.terminate()
            p.wait(timeout=15)
            log.close()

    def main_has(msg):
        def pred():
            r, j = gm.json(["log", "--repo", repo, "--at", "main", "--limit", "1"], cwd=d, check=False)
            return r.rc == 0 and any(msg in c.get("subject", "") for c in j.get("changesets", []))

        return pred

    follow_until(main_has("seed"), "import the seed")
    gm(["line", "create", "batch", "--from", "main", "--repo", repo], cwd=d)

    wc = d / "wc"
    ga(["clone", repo, wc, "--line", "batch"])
    t0 = time.perf_counter()
    for i in range(n):
        f = wc / f"src/mod_{i % 40:03}.py"
        edit_line(f, 1 + 3 * ((i // 40) % 19), f"    return {i} * 7")
        ga(
            ["commit", "-m", f"land {i}: tweak {f.name} @agent-{i % 3}"],
            cwd=wc,
            env={"VOGT_SESSION_ID": SESSION, "VOGT_WORK_ITEM": WORK_ITEM},
        )
        r, _ = ga.json(["land", "--no-check"], cwd=wc)
    land_s = time.perf_counter() - t0

    # Upstream moves on another line of a file the lands touched, so the
    # batch syncs and every replay has to keep it.
    git(["pull", "-q", str(up), "main"], seed)
    edit_line(seed / "src/mod_000.py", 58, "    return 'upstream'")
    git(["commit", "-qam", "upstream: touch mod_000"], seed)
    git(["push", "-q", str(up), "main"], seed)

    r = run([mirror, "pr-batch", "batch", "--base", "main", "--config", cfg], cwd=d, check=False, timeout=3600)
    export_s = r.secs
    (d / "pr-batch.out").write_text(r.out + "\n--- stderr\n" + r.err)
    fake.stop()
    res = {"n": n, "pr_batch_rc": r.rc, "export_s": round(export_s, 3), "lands_s": round(land_s, 3)}
    if r.rc != 0 or not fake.pulls:
        res["error"] = (r.err or r.out)[-1500:]
        return res
    pr = fake.pulls[1]
    branch = pr["head"]
    commits = run(["git", "rev-list", "--reverse", f"main..{branch}"], cwd=up).out.split()
    trailers = {"Graft-Changeset": 0, "Graft-Session": 0, "Graft-Work-Item": 0, "Graft-Batch": 0}
    bound = 0
    at_kept = 0
    for sha in commits:
        msg = run(["git", "log", "-1", "--format=%B", sha], cwd=up).out
        for t in trailers:
            if f"\n{t}: " in msg:
                trailers[t] += 1
        if f"Graft-Session: {SESSION}" in msg and f"Graft-Work-Item: {WORK_ITEM}" in msg:
            bound += 1
        if "@agent-" in msg:
            at_kept += 1
    upstream_kept = "'upstream'" in run(["git", "show", f"{branch}:src/mod_000.py"], cwd=up).out
    tip_tree = run(["git", "rev-parse", f"{branch}^{{tree}}"], cwd=up).out.strip()
    squashed = len(commits) == 1 and n > 1
    res.update(
        {
            "commits": len(commits),
            "squashed": squashed,
            "fallback_reason_in_body": ("squash" in pr.get("body", "").lower()) if squashed else None,
            "trailers": trailers,
            "commits_with_session_and_work_item": bound,
            "commits_keeping_at_sign": at_kept,
            "upstream_change_kept_at_tip": upstream_kept,
            "tip_tree": tip_tree,
            "export_ms_per_land": round(1000 * export_s / n, 2),
        }
    )
    return res


# ---------------------------------------------------------------- identity


def check_identity(site, g_bin: Path, work: Path, repo: str, scopes: list[str]) -> dict:
    """A token bound to a session and work item can't attribute a land to
    another one; a matching or absent claim lands."""
    tok = mint(site, work / "id.token", "bench-identity", "agent", scopes, ["--session", SESSION, "--work-item", WORK_ITEM])
    g = Graft(g_bin, site.url, tok)
    wc = work / "id-wc"
    g(["clone", repo, wc, "--line", LINE])
    out = {}
    cases = [
        ("matching", {"VOGT_SESSION_ID": SESSION, "VOGT_WORK_ITEM": WORK_ITEM}),
        ("other_session", {"VOGT_SESSION_ID": "ses_01OTHER00000000000000000000", "VOGT_WORK_ITEM": WORK_ITEM}),
        ("other_work_item", {"VOGT_SESSION_ID": SESSION, "VOGT_WORK_ITEM": "WI-1"}),
    ]
    for name, env in cases:
        g(["sync"], cwd=wc, check=False)
        edit_line(wc / "src/mod_010.py", 1, f"    return {name!r}")
        c = run([g_bin, "--endpoint", site.url, "--token-file", tok, "commit", "-m", f"identity {name}"], cwd=wc, env=env, check=False)
        lr = (
            run([g_bin, "--endpoint", site.url, "--token-file", tok, "land", "--no-check"], cwd=wc, env=env, check=False)
            if c.rc == 0
            else None
        )
        text = c.err + c.out + ((lr.err + lr.out) if lr else "")
        rule = next((r for r in ("session-metadata", "work-item-metadata") if r in text), None)
        out[name] = {"commit_rc": c.rc, "land_rc": lr.rc if lr else None, "rule": rule}
        if c.rc != 0 or (lr and lr.rc != 0):
            # drop the refused local changeset before the next case
            shutil.rmtree(wc)
            g(["clone", repo, wc, "--line", LINE])
    out["pass"] = (
        out["matching"]["land_rc"] == 0
        and out["other_session"]["rule"] == "session-metadata"
        and out["other_work_item"]["rule"] == "work-item-metadata"
    )
    return out


# ---------------------------------------------------------------- main


def main(argv=None):
    p = argparse.ArgumentParser(prog="graftbench.phase1", description=__doc__)
    p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--k", type=int, default=20, help="samples per pre-land case (default 20)")
    p.add_argument("--batches", default="10,50,200,500", help="batch sizes N (default 10,50,200,500)")
    p.add_argument("--keep", action="store_true")
    p.add_argument("--line", default="main", help="line for the identity and pre-land cases, forked from main if not main")
    p.add_argument("--prefix", default="p1", help="repository name prefix (default p1)")
    p.add_argument("--container", help="run against this running graft-server container (docker exec for admin)")
    p.add_argument("--url", default="http://127.0.0.1:17888", help="its URL, with --container")
    a = p.parse_args(argv)
    bins = a.bin_dir.resolve()
    a.out.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="graft-p1-"))
    global LINE
    LINE = a.line
    P = a.prefix
    if a.container:
        site = DeployedSite(a.container, a.url, work / "site")
        site.admin(["repo", "create", P])
    else:
        site = LocalSite(bins / "graft-server", work / "site", [P])
    res: dict = {
        "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "graft": run([bins / "graft", "version"], check=False).out.strip(),
        "host": os.uname().nodename,
        "nproc": os.cpu_count(),
        "builds_or_tests_run": 0,
        "loadavg_start": os.getloadavg(),
        "site": a.container or "local",
    }
    try:
        seed = work / "seed-p1"
        run(["git", "init", "-q", "-b", "main", seed])
        write_files(seed, project_files())
        git(["add", "-A"], seed)
        git(["commit", "-qm", "seed"], seed)
        run(["git", "clone", "-q", "--bare", seed, work / "seed-p1.git"])
        Mirror(bins / "graft-mirror", site.url, site.token_file).import_git(work / "seed-p1.git", P, work / "seed-p1.sqlite")

        if LINE != "main":
            Graft(bins / "graft", site.url, site.token_file)(["line", "create", LINE, "--from", "main", "--repo", P], cwd=work)

        def admin_scopes_for(repo):
            return [f"repo:{repo}:read", f"repo:{repo}:write", f"repo:{repo}:admin", f"repo:{repo}:land:batch", f"repo:{repo}:land:main"]

        res["identity"] = check_identity(site, bins / "graft", work, P, [f"repo:{P}:read", f"repo:{P}:write", f"repo:{P}:land:{LINE}"])
        tok = mint(
            site,
            work / "pl.token",
            "bench-preland",
            "agent",
            [f"repo:{P}:read", f"repo:{P}:write", f"repo:{P}:land:{LINE}", f"repo:{P}:drafts"],
        )
        res["preland"] = bench_preland(site, bins / "graft", work, P, tok, a.k)
        res["batches"] = []
        for n in [int(x) for x in a.batches.split(",") if x]:
            res["batches"].append(bench_batch(site, bins, work, n, admin_scopes_for, P))
            (a.out / "phase1.json").write_text(json.dumps(res, indent=2))
        res["server"] = site.stats()
        res["loadavg_end"] = os.getloadavg()
    finally:
        res["finished"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        (a.out / "phase1.json").write_text(json.dumps(res, indent=2))
        site.stop()
        if not a.keep:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"kept {work}")
    print(json.dumps(res, indent=2))


if __name__ == "__main__":
    main()
