"""Batch delivery: N agents land on an integration line, `graft-mirror
pr-batch` exports one commit per land (D26) to a local bare "GitHub"
repository behind a fake GitHub API, one planted bad land breaks the build,
the operator drops it, and a simulated rebase-merge lands the batch on
`main` (D28). WI-2352.

Measured:
- authorship: each exported commit's author, `Graft-Session`,
  `Graft-Work-Item` and `Graft-Changeset` against the land that made it, and
  the author kind on graft's changeset;
- history: commits on `main` per land, and `git blame` and `git bisect` on
  the export pointing at the right land's commit;
- blast radius: good lands held in the batch the bad land broke, and the
  time to drop it and recut;
- latency: land to `main`, p50 and p90, with the merge step simulated.

The "build" is `python3 -m compileall` over `src/`, run on the batch head,
standing in for the forge's CI (graft itself runs no checks, D22). The
pre-land check in `graft land` (D27) is on, as it is by default.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from .proc import StepError, percentile, run
from .site import LocalSite
from .tools import Git

REPO = "batchbench"
LINE = "int"
OWNER, GH_REPO = "bench", "upstream"
PAD = 6
MAX_ATTEMPTS = 30


# --- fake GitHub ---------------------------------------------------------


class FakeGitHub:
    """The REST and GraphQL calls `pr-batch` makes, over a bare repository.
    `merge(n)` simulates oversight's `land-pr` rebase-merge: `main` moves to
    the PR head (a fast-forward; the base never moves in this bench)."""

    def __init__(self, bare: Path, git: Git):
        self.bare = bare
        self.git = git
        self.pulls: dict[int, dict] = {}
        self.lock = threading.Lock()
        self.calls = 0
        outer = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def _send(self, code, body):
                data = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def _body(self):
                n = int(self.headers.get("Content-Length") or 0)
                return json.loads(self.rfile.read(n) or b"{}")

            def do_GET(self):
                outer.calls += 1
                u = urlparse(self.path)
                base = f"/repos/{OWNER}/{GH_REPO}/pulls"
                if u.path == base:
                    head = parse_qs(u.query).get("head", [""])[0].split(":", 1)[-1]
                    with outer.lock:
                        found = [outer.view(p) for p in outer.pulls.values() if not head or p["head"] == head]
                    return self._send(200, found)
                m = re.fullmatch(base + r"/(\d+)(/reviews)?", u.path)
                if m and int(m.group(1)) in outer.pulls:
                    if m.group(2):
                        return self._send(200, [])
                    with outer.lock:
                        return self._send(200, outer.view(outer.pulls[int(m.group(1))]))
                if u.path.endswith("/check-runs"):
                    return self._send(200, {"total_count": 0, "check_runs": []})
                self._send(404, {"message": "Not Found"})

            def do_POST(self):
                outer.calls += 1
                u = urlparse(self.path)
                body = self._body()
                if u.path == "/graphql":
                    return self._send(200, {"data": {"node": {"isInMergeQueue": False, "autoMergeRequest": None}}})
                if u.path == f"/repos/{OWNER}/{GH_REPO}/pulls":
                    with outer.lock:
                        n = len(outer.pulls) + 1
                        outer.pulls[n] = {
                            "number": n,
                            "head": body["head"],
                            "base": body["base"],
                            "title": body.get("title", ""),
                            "body": body.get("body", ""),
                            "state": "open",
                            "merged": False,
                            "merge_commit_sha": None,
                            "opened": time.time(),
                            "comments": [],
                        }
                        return self._send(201, outer.view(outer.pulls[n]))
                m = re.fullmatch(rf"/repos/{OWNER}/{GH_REPO}/issues/(\d+)/comments", u.path)
                if m:
                    with outer.lock:
                        outer.pulls[int(m.group(1))]["comments"].append(body.get("body", ""))
                    return self._send(201, {"id": 1})
                self._send(404, {"message": "Not Found"})

            def do_PATCH(self):
                outer.calls += 1
                u = urlparse(self.path)
                body = self._body()
                m = re.fullmatch(rf"/repos/{OWNER}/{GH_REPO}/pulls/(\d+)", u.path)
                if m:
                    with outer.lock:
                        p = outer.pulls[int(m.group(1))]
                        if body.get("state") == "closed":
                            p["state"] = "closed"
                            p["closed"] = time.time()
                        return self._send(200, outer.view(p))
                self._send(404, {"message": "Not Found"})

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def head_sha(self, branch: str) -> str:
        r = self.git(["rev-parse", "--verify", "-q", f"refs/heads/{branch}"], cwd=self.bare, check=False)
        return r.out.strip() or "0" * 40

    def view(self, p: dict) -> dict:
        return {
            "number": p["number"],
            "html_url": f"https://github.invalid/{OWNER}/{GH_REPO}/pull/{p['number']}",
            "node_id": f"PR_{p['number']}",
            "state": p["state"],
            "merged": p["merged"],
            "merge_commit_sha": p["merge_commit_sha"],
            "head": {"sha": self.head_sha(p["head"]), "ref": p["head"]},
        }

    def merge(self, n: int) -> str:
        with self.lock:
            p = self.pulls[n]
            sha = self.head_sha(p["head"])
            self.git(["update-ref", "refs/heads/main", sha], cwd=self.bare)
            p.update(state="closed", merged=True, merge_commit_sha=sha, merged_at=time.time())
            return sha

    def stop(self):
        self.httpd.shutdown()


# --- the scenario ---------------------------------------------------------


def shared_text(values: dict[int, list[int]], agents: int) -> str:
    out = ['"""Shared registry: each agent owns one region."""', ""]
    for i in range(agents):
        out.append(f"AGENT_{i} = {sorted(values.get(i, []))}")
        out += [f"# region {i} line {j}" for j in range(PAD)]
    return "\n".join(out) + "\n"


def agent_text(i: int, lands: list[int], bad: bool) -> str:
    out = [f'"""Agent {i}\'s module."""', ""]
    for k in lands:
        out += [f"def f_{i}_{k}():", f"    return {k}", ""]
    if bad:
        out += ["def broken(:", "    pass", ""]
    return "\n".join(out)


def build_ok(git: Git, repo: Path, rev: str, scratch: Path) -> bool:
    """The forge's CI stand-in: compileall over `src/` at `rev`."""
    d = scratch / f"ci-{rev[:12]}"
    shutil.rmtree(d, ignore_errors=True)
    d.mkdir(parents=True)
    tar = subprocess.run(["git", "-C", str(repo), "archive", rev], capture_output=True, check=True).stdout
    subprocess.run(["tar", "-x", "-C", str(d)], input=tar, check=True)
    ok = subprocess.run(["python3", "-m", "compileall", "-q", "src"], cwd=d, capture_output=True).returncode == 0
    shutil.rmtree(d, ignore_errors=True)
    return ok


def trailers(msg: str) -> dict:
    t = {}
    for line in msg.splitlines():
        m = re.match(r"^(Graft-[A-Za-z-]+): (.*)$", line)
        if m:
            t[m.group(1)] = m.group(2).strip()
    return t


def commits(git: Git, repo: Path, rng: str) -> list[dict]:
    """Oldest first: sha, author name and email, trailers."""
    out = git(["log", "--reverse", "--format=%H%x00%an%x00%ae%x00%B%x1e", rng], cwd=repo).out
    res = []
    for rec in out.split("\x1e"):
        rec = rec.strip("\n")
        if not rec:
            continue
        sha, an, ae, body = rec.split("\x00", 3)
        res.append({"sha": sha, "name": an, "email": ae, "trailers": trailers(body), "subject": body.splitlines()[0]})
    return res


class Agent:
    def __init__(self, i: int, ctx: "Bench"):
        self.i = i
        self.ctx = ctx
        self.session = f"ses_bench{i:02d}"
        self.work_item = f"WI-90{i:02d}"
        self.name = f"agent-{i}"
        self.email = f"agent-{i}@bench.invalid"
        self.wc = ctx.work / f"wc-{i}"
        self.token = ctx.mint(
            f"agent-{i}",
            "agent",
            ["read", "write", f"land:{LINE}"],
            ["--session", self.session, "--work-item", self.work_item],
        )
        self.lands: list[int] = []

    def env(self) -> dict:
        return {
            "VOGT_SESSION_ID": self.session,
            "VOGT_WORK_ITEM": self.work_item,
            "GRAFT_AUTHOR_NAME": self.name,
            "GRAFT_AUTHOR_EMAIL": self.email,
        }

    def graft(self, args, check=True):
        c = self.ctx
        return run(
            [c.graft_bin, "--endpoint", c.site.url, "--token-file", self.token] + args,
            cwd=self.wc,
            env=self.env(),
            check=check,
        )

    def clone(self):
        c = self.ctx
        run(
            [c.graft_bin, "--endpoint", c.site.url, "--token-file", self.token, "clone", REPO, self.wc, "--line", LINE],
            env=self.env(),
        )

    def land_once(self, k: int, bad: bool) -> dict:
        """Edit own module and own region of shared.py, commit, land (pre-land
        check on); exit 3 resolves by `land --sync`, exit 4 retries."""
        self.graft(["sync"], check=False)
        self.lands.append(k)
        (self.wc / "src" / f"agent_{self.i}.py").write_text(agent_text(self.i, self.lands, bad))
        shared = self.wc / "src" / "shared.py"
        text = shared.read_text()
        text = re.sub(rf"^AGENT_{self.i} = .*$", f"AGENT_{self.i} = {sorted(self.lands)}", text, flags=re.M)
        shared.write_text(text)
        msg = f"agent {self.i}: land {k}" + (" (planted: breaks the build)" if bad else "")
        self.graft(["commit", "-m", msg])
        tries = 0
        conflicts = 0
        t0 = time.perf_counter()
        while True:
            tries += 1
            r = self.graft(["land", "--json"], check=False)
            if r.rc == 0:
                j = json.loads(r.out)
                return {
                    "agent": self.i,
                    "k": k,
                    "bad": bad,
                    "id": j["landed"],
                    "landed_at": time.time(),
                    "land_s": time.perf_counter() - t0,
                    "tries": tries,
                    "conflicts": conflicts,
                    "session": self.session,
                    "work_item": self.work_item,
                    "name": self.name,
                    "email": self.email,
                }
            if tries >= MAX_ATTEMPTS:
                raise StepError(f"agent {self.i} land {k}: gave up after {tries} tries: {r.err[-500:]}")
            if r.rc == 3:
                conflicts += 1
                s = self.graft(["land", "--sync", "--json"], check=False)
                if s.rc == 0:
                    j = json.loads(s.out)
                    return {
                        "agent": self.i,
                        "k": k,
                        "bad": bad,
                        "id": j["landed"],
                        "landed_at": time.time(),
                        "land_s": time.perf_counter() - t0,
                        "tries": tries + 1,
                        "conflicts": conflicts,
                        "session": self.session,
                        "work_item": self.work_item,
                        "name": self.name,
                        "email": self.email,
                    }
            time.sleep(0.05)


class Bench:
    def __init__(self, graft_bin: Path, server_bin: Path, mirror_bin: Path, work: Path, log):
        self.graft_bin = graft_bin
        self.server_bin = server_bin
        self.mirror_bin = mirror_bin
        self.work = work
        self.log = log
        self.git = Git()

    def mint(self, subject: str, kind: str, scopes: list[str], extra: list[str] | None = None) -> Path:
        args = ["token", "mint", "--subject", subject, "--kind", kind, "--ttl", "86400"]
        for s in scopes:
            args += ["--scope", f"repo:{REPO}:{s}"]
        out = self.site.admin(args + (extra or [])).out.strip()
        path = self.work / f"{subject}.token"
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(out + "\n")
        return path

    def pr_batch(self, *extra) -> tuple[float, str]:
        r = run([self.mirror_bin, "pr-batch", LINE, "--base", "main", "--config", self.config] + list(extra), check=False)
        if r.rc != 0:
            raise StepError(f"pr-batch {' '.join(extra)}: exit {r.rc}\n{r.out}\n{r.err[-1500:]}")
        return r.secs, r.out


def run_mode(
    graft_bin, server_bin, mirror_bin, work: Path, log, agents: int = 5, rounds: int = 2, bad_at: tuple[int, int] | None = None
) -> dict:
    # The bad land is agent 1's last: a drop of a land a later land builds on
    # conflicts (fixed forward instead), which the first bench run showed.
    bad_at = bad_at or (1, rounds - 1)
    b = Bench(graft_bin, server_bin, mirror_bin, work, log)
    git = b.git
    # Upstream "GitHub": a bare repo with the project on main.
    up = work / "github.git"
    git(["init", "-q", "--bare", "-b", "main", str(up)])
    seed = work / "seed"
    git(["init", "-q", "-b", "main", str(seed)])
    (seed / "src").mkdir()
    (seed / "src" / "shared.py").write_text(shared_text({}, agents))
    for i in range(agents):
        (seed / "src" / f"agent_{i}.py").write_text(agent_text(i, [], False))
    (seed / "README.md").write_text("batch bench project\n")
    git(["add", "-A"], cwd=seed)
    git(["commit", "-q", "-m", "init"], cwd=seed)
    git(["push", "-q", str(up), "main"], cwd=seed)

    b.site = LocalSite(server_bin, work / "server", [REPO])
    gh = FakeGitHub(up, git)
    try:
        mtok = b.mint("graft-mirror", "service", ["read", "write", "admin", f"land:{LINE}", "land:main"])
        ghtok = work / "github.token"
        fd = os.open(ghtok, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write("fake-github-token\n")
        b.config = work / "mirror.toml"
        b.config.write_text(
            f'server = "{b.site.url}"\nrepo = "{REPO}"\nupstream = "{up}"\n'
            f'mirror_dir = "{work / "mirror.git"}"\ndb = "{work / "mirror.sqlite"}"\n'
            f'token_file = "{mtok}"\n\n[[lines]]\nbranch = "main"\nline = "main"\n\n'
            f'[github]\napi = "{gh.url}"\nowner = "{OWNER}"\nrepo = "{GH_REPO}"\ntoken_file = "{ghtok}"\n'
        )
        # First run follows main and stops: the integration line does not exist yet.
        run([mirror_bin, "pr-batch", LINE, "--base", "main", "--config", b.config], check=False)
        boot = b.mint("bootstrap", "human", ["read", "write", "admin", f"land:{LINE}"])
        boot_wc = work / "boot"
        run([graft_bin, "--endpoint", b.site.url, "--token-file", boot, "clone", REPO, boot_wc, "--line", "main"])
        tip = json.loads(
            run([graft_bin, "--endpoint", b.site.url, "--token-file", boot, "log", "--limit", "1", "--json"], cwd=boot_wc).out
        )["changesets"][0]["id"]
        run([graft_bin, "--endpoint", b.site.url, "--token-file", boot, "line", "create", LINE, "--from", tip], cwd=boot_wc)
        log(f"[batch] line {LINE} forked at {tip[:13]}; {agents} agents x {rounds} lands")

        ags = [Agent(i, b) for i in range(agents)]
        for a in ags:
            a.clone()

        # Concurrent lands: every agent lands once per round, at the same time.
        lands: list[dict] = []
        errors: list[str] = []
        lk = threading.Lock()
        for k in range(rounds):
            bar = threading.Barrier(agents)

            def go(a: Agent, k=k, bar=bar):
                try:
                    bar.wait()
                    r = a.land_once(k, (a.i, k) == bad_at)
                    with lk:
                        lands.append(r)
                except Exception as e:  # noqa: BLE001
                    with lk:
                        errors.append(str(e))

            ts = [threading.Thread(target=go, args=(a,)) for a in ags]
            for t in ts:
                t.start()
            for t in ts:
                t.join()
            if errors:
                raise StepError("; ".join(errors))
        # Land order is the line's first-parent order.
        order = [
            c["id"]
            for c in reversed(
                json.loads(
                    run(
                        [graft_bin, "--endpoint", b.site.url, "--token-file", boot, "log", "--at", LINE, "--limit", "200", "--json"],
                        cwd=boot_wc,
                        check=False,
                    ).out
                    or "{}"
                ).get("changesets", [])
            )
        ]
        lands.sort(key=lambda r: order.index(r["id"]) if r["id"] in order else 10**9)
        bad = next(r for r in lands if r["bad"])
        log(f"[batch] {len(lands)} lands; bad land {bad['id'][:13]} (agent {bad['agent']})")

        # Author kind on graft's changesets.
        kinds = {}
        for r in lands:
            cs = json.loads(run([graft_bin, "--endpoint", b.site.url, "--token-file", boot, "show", r["id"], "--json"], cwd=boot_wc).out)
            cs = cs.get("changeset", cs)
            kinds[r["id"]] = (cs.get("author") or {}).get("kind")

        # Cut batch 1.
        t_cut1 = time.time()
        cut1_s, out1 = b.pr_batch()
        head1 = gh.head_sha("graft/batch-1")
        base = git(["rev-parse", "main"], cwd=up).out.strip()
        c1 = commits(git, up, f"{base}..{head1}")
        ci1 = build_ok(git, up, head1, work)

        # Authorship of batch 1.
        by_id = {r["id"]: r for r in lands}
        auth = []
        for c in c1:
            r = by_id.get(c["trailers"].get("Graft-Changeset"))
            auth.append(
                {
                    "sha": c["sha"],
                    "changeset": c["trailers"].get("Graft-Changeset"),
                    "author_ok": bool(r) and c["name"] == r["name"] and c["email"] == r["email"],
                    "session_ok": bool(r) and c["trailers"].get("Graft-Session") == r["session"],
                    "work_item_ok": bool(r) and c["trailers"].get("Graft-Work-Item") == r["work_item"],
                    "kind_ok": bool(r) and kinds.get(r["id"]) == "agent",
                }
            )
        all_ok = [all(v for k, v in a.items() if k.endswith("_ok")) for a in auth]

        # Bisect and blame on the export.
        clone = work / "export-clone"
        git(["clone", "-q", str(up), str(clone)])
        git(["fetch", "-q", "origin", "graft/batch-1:b1"], cwd=clone)
        script = work / "build.sh"
        script.write_text("#!/bin/sh\ngit clean -fdxq\nexec python3 -m compileall -f -q src >/dev/null\n")
        script.chmod(0o755)
        git(["bisect", "start", "b1", base], cwd=clone)
        bis = git(["bisect", "run", str(script)], cwd=clone, check=False).out
        m = re.search(r"^([0-9a-f]{40}) is the first bad commit", bis, re.M)
        first_bad = m.group(1) if m else None
        git(["bisect", "reset"], cwd=clone, check=False)
        sha_of = {c["trailers"].get("Graft-Changeset"): c["sha"] for c in c1}
        bisect_ok = first_bad is not None and first_bad == sha_of.get(bad["id"])
        blame_hits = 0
        blame_total = 0
        for r in lands:
            if r["bad"]:
                continue
            blame = git(["blame", "--porcelain", "b1", "--", f"src/agent_{r['agent']}.py"], cwd=clone).out
            # The line `return <k>` of f_<agent>_<k> was added by that land.
            lines = blame.splitlines()
            for idx, ln in enumerate(lines):
                if ln == f"\t    return {r['k']}":
                    # find the header for this line: scan back to the last sha line
                    j = idx - 1
                    while j >= 0 and not re.match(r"^[0-9a-f]{40} ", lines[j]):
                        j -= 1
                    blame_total += 1
                    if j >= 0 and lines[j].split()[0] == sha_of.get(r["id"]):
                        blame_hits += 1
                    break

        # Drop the bad land and recut.
        t_drop = time.time()
        drop_s, out2 = b.pr_batch("--exclude", bad["id"], "--reason", "breaks the build (bench)")
        t_recut = time.time()
        head2 = gh.head_sha("graft/batch-2")
        c2 = commits(git, up, f"{base}..{head2}") if head2 != "0" * 40 else []
        ci2 = build_ok(git, up, head2, work) if c2 else False
        pr1 = gh.pulls.get(1, {})

        # Simulated review + land-pr: rebase-merge batch 2, then settle.
        t_merge = time.time()
        merged = gh.merge(2) if c2 else None
        settle_s, out3 = b.pr_batch()
        t_settled = time.time()
        main_commits = commits(git, up, f"{base}..main")
        good = [r for r in lands if not r["bad"]]
        lat = [t_settled - r["landed_at"] for r in good]
        on_main = {c["trailers"].get("Graft-Changeset") for c in main_commits}
        res = {
            "agents": agents,
            "rounds": rounds,
            "lands": len(lands),
            "land_tries": sum(r["tries"] for r in lands),
            "land_conflicts": sum(r["conflicts"] for r in lands),
            "land_p50_ms": round(1000 * percentile([r["land_s"] for r in lands], 50), 1),
            "land_p90_ms": round(1000 * percentile([r["land_s"] for r in lands], 90), 1),
            "bad_land": bad["id"],
            "batch1": {
                "commits": len(c1),
                "per_land": len(c1) == len(lands),
                "ci_passed": ci1,
                "cut_s": round(cut1_s, 3),
                "pr_open_after_last_land_s": round(t_cut1 + cut1_s - max(r["landed_at"] for r in lands), 3),
            },
            "authorship": {
                "commits": len(auth),
                "fully_correct": sum(all_ok),
                "share": round(sum(all_ok) / len(auth), 3) if auth else 0,
                "author": sum(a["author_ok"] for a in auth),
                "session": sum(a["session_ok"] for a in auth),
                "work_item": sum(a["work_item_ok"] for a in auth),
                "kind_agent_on_graft": sum(a["kind_ok"] for a in auth),
            },
            "history": {
                "bisect_first_bad_is_bad_land": bisect_ok,
                "blame_good_lands_correct": f"{blame_hits}/{blame_total}",
                "main_commits": len(main_commits),
                "good_lands_on_main": sum(1 for r in good if r["id"] in on_main),
                "good_lands": len(good),
                "main_commits_per_land": round(len(main_commits) / len(lands), 3),
                "bad_land_and_revert_both_on_main": bad["id"] in on_main
                and any(c["subject"].lower().startswith("revert") for c in main_commits),
            },
            "blast_radius": {
                "good_lands_held_by_bad": sum(1 for c in c1 if c["trailers"].get("Graft-Changeset") != bad["id"]),
                "drop_and_recut_s": round(drop_s, 3),
                "pr1_closed": pr1.get("state") == "closed",
                "pr1_comment": bool(pr1.get("comments")),
                "batch2_commits": len(c2),
                "batch2_ci_passed": ci2,
            },
            "latency": {
                "merge_simulated": "immediate rebase-merge (fast-forward) after the recut; no review wait",
                "land_to_main_p50_s": round(percentile(lat, 50), 3),
                "land_to_main_p90_s": round(percentile(lat, 90), 3),
                "settle_s": round(settle_s, 3),
                "merged_sha": merged,
            },
            "fake_github_calls": gh.calls,
            "outputs": {"cut1": out1.strip(), "drop": out2.strip(), "settle": out3.strip()},
            "drop_window_s": round(t_recut - t_drop, 3),
            "merge_to_settled_s": round(t_settled - t_merge, 3),
        }
        res["ok"] = (
            res["batch1"]["per_land"]
            and res["authorship"]["share"] == 1.0
            and not ci1
            and bisect_ok
            and blame_hits == blame_total
            and ci2
            and res["history"]["good_lands_on_main"] == len(good)
        )
        return res
    finally:
        gh.stop()
        b.site.stop()


def markdown(r: dict) -> list[str]:
    if "error" in r:
        return ["## Batch delivery", "", "```", r["error"], "```", ""]
    a, h, br, lt, b1 = r["authorship"], r["history"], r["blast_radius"], r["latency"], r["batch1"]
    return [
        "## Batch delivery (WI-2352)",
        "",
        f"{r['agents']} agents x {r['rounds']} lands = {r['lands']} lands on `{LINE}`, concurrent, pre-land check on; "
        f"{r['land_conflicts']} lands needed `--sync`, {r['land_tries']} land attempts; land p50 {r['land_p50_ms']} ms, p90 {r['land_p90_ms']} ms.",
        "",
        "| Measure | Result |",
        "| --- | --- |",
        f"| Batch 1 commits | {b1['commits']} for {r['lands']} lands (one per land: {b1['per_land']}); CI {'pass' if b1['ci_passed'] else 'fail (planted)'} |",
        f"| Authorship fully correct | {a['fully_correct']}/{a['commits']} ({a['share']:.0%}): author {a['author']}, session {a['session']}, work item {a['work_item']}, kind agent {a['kind_agent_on_graft']} |",
        f"| `git bisect` lands on the bad land's commit | {h['bisect_first_bad_is_bad_land']} |",
        f"| `git blame` of good lands' lines | {h['blame_good_lands_correct']} on the right land's commit |",
        f"| Commits on `main` | {h['main_commits']} for {r['lands']} lands ({h['main_commits_per_land']} per land; the bad land and its revert are both kept: {h['bad_land_and_revert_both_on_main']}) |",
        f"| Good lands held by the bad one | {br['good_lands_held_by_bad']} |",
        f"| Drop and recut (`pr-batch --exclude`) | {br['drop_and_recut_s']} s; PR 1 closed {br['pr1_closed']} with comment {br['pr1_comment']}; batch 2 {br['batch2_commits']} commits, CI {'pass' if br['batch2_ci_passed'] else 'FAIL'} |",
        f"| Cut batch 1 | {b1['cut_s']} s (PR open {b1['pr_open_after_last_land_s']} s after the last land) |",
        f"| Land to `main` ({lt['merge_simulated']}) | p50 {lt['land_to_main_p50_s']} s, p90 {lt['land_to_main_p90_s']} s; settle run {lt['settle_s']} s |",
        "",
        f"Result: {'pass' if r['ok'] else 'FAIL'}.",
        "",
    ]
