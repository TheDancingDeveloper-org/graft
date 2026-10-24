"""Real overlapping PRs from the estate, replayed through graft and git (WI-2338).

The bench's other scenarios use a generated project. This one takes real PR
diffs from the repositories in docs/notes/graft-fit-assessment.md (gap 4) and
lands each from the fork point it actually had, once with `graft land` and
once with git (`git merge-tree` to classify, `git rebase` and `git push` to
land). It reports, per case and per tool: clean lands, conflicts,
clean-but-broken merges, client round trips and wall time.

Two subcommands, run from bench/:

    python3 -m graftbench.realprs fetch   # GitHub metadata -> realprs/cases.json
    python3 -m graftbench.realprs run     # replay -> results/<date>-real-prs/

What a PR's change is. GitHub keeps only a PR's last head, and many were
rebased by force push or had `main` merged back in. The replay uses the
change the agent had before it first had to integrate:
- the oldest `HeadRefForcePushedEvent.beforeCommit`, when it was force-pushed;
- else the first parent of the first commit that merged another branch in;
- else the head.
Its fork is that commit's merge-base with `main`.

Three modes:
- `position`: each PR lands on its own line, which starts where `main` was
  when the PR really integrated (merged PRs: the squash commit's parent; PRs
  that merged `main` back in: the `main` commit they merged). The contention
  is everything that really landed between the fork and that point.
- `sequence`: open PRs land one after another on one line that starts at
  today's `main`, each from its own fork.
- `stack`: a chain of PRs (each based on the one before) lands onto today's
  `main`: graft commits the chain as drafts in one workspace and lands once;
  git lands them one by one, rebasing the rest of the chain each time.

The oracle for "clean but broken" is `cargo check --workspace --all-targets`
on the merged tree (CARGO_BUILD_JOBS=4). A merge is broken when its tree fails
and both of its inputs (the PR's own tree and the line before it) pass. This
is a measurement. It is never a land check or gate (D22).
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

from .proc import StepError, percentile, run
from .site import LocalSite
from .tools import GIT_ENV, Git, Graft, Mirror

ROOT = Path(__file__).resolve().parents[2]
BENCH = ROOT / "bench"
CASES_FILE = BENCH / "realprs" / "cases.json"
OWNER = "TheDancingDeveloper-org"
MAX_ATTEMPTS = 20

# The cases, as the assessment names them.
CASES = [
    {
        "id": "rdpapp-split",
        "repo": "rdpapp",
        "mode": "sequence",
        "what": "rdpapp `main.rs` split: the open PRs among #38-#50 that move gateway handlers out of main.rs",
        "select": {"numbers": list(range(38, 51)), "state": "OPEN", "touches": ["crates/rdpapp-gateway/src/main.rs"]},
    },
    {
        "id": "rdpapp-stack",
        "repo": "rdpapp",
        "mode": "stack",
        "what": "rdpapp 11-PR stack #28 -> #51 -> ... -> #59, plus #60 (merged into #59)",
        "select": {"chain": [28, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60]},
    },
    {
        "id": "fluent2-registration",
        "repo": "fluent2-egui",
        "mode": "position",
        "what": "fluent2-egui PRs touching CHANGELOG.md or gallery/src/pages/mod.rs while the queue-ejected #58, #60, #64 and #87 were open",
        "select": {"window_of": [58, 60, 64, 87], "touches": ["CHANGELOG.md", "gallery/src/pages/mod.rs"], "state": "MERGED"},
    },
    {
        "id": "rusttorrent-qbit",
        "repo": "rustTorrent",
        "mode": "position",
        "what": "rustTorrent qbit cluster #138, #139, #141 against #150",
        "select": {"numbers": [138, 139, 141, 150], "state": "MERGED"},
    },
    {
        "id": "graft-merge-back",
        "repo": "graft",
        "mode": "position",
        "what": "graft PRs that merged `main` back in, landed at the `main` they merged",
        "select": {"merged_back": True},
    },
]

PR_QUERY = """query($o:String!,$n:String!,$c:String){repository(owner:$o,name:$n){
pullRequests(first:25,after:$c,orderBy:{field:CREATED_AT,direction:ASC}){pageInfo{hasNextPage endCursor}
nodes{number baseRefName headRefName headRefOid state mergedAt createdAt
mergeCommit{oid parents(first:2){nodes{oid}}}
commits(first:100){nodes{commit{oid parents(first:2){nodes{oid}}}}}
files(first:100){nodes{path}}
timelineItems(first:50,itemTypes:[HEAD_REF_FORCE_PUSHED_EVENT]){nodes{... on HeadRefForcePushedEvent{createdAt beforeCommit{oid}}}}}}}}"""


# ---------------------------------------------------------------- selection


def first_merge(pr: dict) -> dict | None:
    """The PR's first commit with two parents, if any."""
    for node in pr["commits"]["nodes"]:
        c = node["commit"]
        if len(c["parents"]["nodes"]) > 1:
            return c
    return None


def original_head(pr: dict) -> tuple[str, str]:
    """The head the agent had before it first integrated, and why."""
    pushes = [n for n in pr.get("timelineItems", {}).get("nodes", []) if n and n.get("beforeCommit")]
    if pushes:
        pushes.sort(key=lambda n: n["createdAt"])
        return pushes[0]["beforeCommit"]["oid"], "before-force-push"
    m = first_merge(pr)
    if m:
        return m["parents"]["nodes"][0]["oid"], "before-merge-back"
    return pr["headRefOid"], "head"


def paths(pr: dict) -> set[str]:
    return {f["path"] for f in pr["files"]["nodes"]}


def select(case: dict, prs: dict[int, dict]) -> list[dict]:
    """The case's PRs, in land order."""
    s = case["select"]
    if "chain" in s:
        return [prs[n] for n in s["chain"]]
    if "window_of" in s:
        ej = [prs[n] for n in s["window_of"]]
        lo = min(p["createdAt"] for p in ej)
        hi = max(p["mergedAt"] for p in ej)
        out = [p for p in prs.values() if p["mergedAt"] and p["createdAt"] <= hi and p["mergedAt"] >= lo and paths(p) & set(s["touches"])]
    elif s.get("merged_back"):
        out = [p for p in prs.values() if p["mergedAt"] and first_merge(p)]
    else:
        out = [prs[n] for n in s["numbers"] if n in prs]
    if "state" in s:
        out = [p for p in out if p["state"] == s["state"]]
    if "touches" in s and "window_of" not in s:
        out = [p for p in out if paths(p) & set(s["touches"])]
    key = (lambda p: (p["mergedAt"], p["number"])) if case["mode"] == "position" else (lambda p: p["number"])
    return sorted(out, key=key)


# ---------------------------------------------------------------- git helpers


def git(repo: Path, *args, check: bool = True, input: bytes | None = None, env: dict | None = None):
    e = dict(GIT_ENV)
    if env:
        e.update(env)
    return run(["git", "-C", repo, *args], env=e, check=check, input=input, timeout=600)


def rev(repo: Path, r: str) -> str:
    return git(repo, "rev-parse", "--verify", "-q", r + "^{commit}").out.strip()


def is_ancestor(repo: Path, a: str, b: str) -> bool:
    return git(repo, "merge-base", "--is-ancestor", a, b, check=False).rc == 0


def gh_git(repo: Path, *args) -> None:
    """A git network command, authenticated by gh's credential helper (no token in argv)."""
    run(["git", "-C", repo, "-c", "credential.helper=", "-c", "credential.helper=!gh auth git-credential", *args], timeout=1800)


def ensure_cache(cache: Path, name: str) -> Path:
    """A bare clone of the repository with every PR head."""
    bare = cache / f"{name}.git"
    url = f"https://github.com/{OWNER}/{name}.git"
    if not bare.exists():
        cache.mkdir(parents=True, exist_ok=True)
        run(["git", "-c", "credential.helper=!gh auth git-credential", "clone", "-q", "--bare", url, bare], timeout=3600)
    gh_git(bare, "fetch", "-q", url, "+refs/heads/main:refs/heads/main", "+refs/pull/*/head:refs/pull/*/head")
    return bare


def ensure_commit(bare: Path, sha: str) -> bool:
    """Fetches a commit no ref points at (a force-pushed head) by id."""
    if git(bare, "cat-file", "-e", sha + "^{commit}", check=False).rc == 0:
        return True
    try:
        gh_git(bare, "fetch", "-q", f"https://github.com/{OWNER}/{bare.stem}.git", sha)
    except StepError:
        return False
    return git(bare, "cat-file", "-e", sha + "^{commit}", check=False).rc == 0


# ---------------------------------------------------------------- fetch


def gh_prs(repo: str) -> list[dict]:
    out, cursor = [], None
    while True:
        args = ["gh", "api", "graphql", "-f", f"query={PR_QUERY}", "-F", f"o={OWNER}", "-F", f"n={repo}"]
        if cursor:
            args += ["-F", f"c={cursor}"]
        d = json.loads(run(args, timeout=300).out)["data"]["repository"]["pullRequests"]
        out += d["nodes"]
        if not d["pageInfo"]["hasNextPage"]:
            return out
        cursor = d["pageInfo"]["endCursor"]


def plan_case(case: dict, prs: dict[int, dict], bare: Path, main_tip: str) -> dict:
    """Resolves each PR to (fork, head, target) commits."""
    entries, skipped = [], []
    for p in select(case, prs):
        n = p["number"]
        if case["mode"] == "position":
            head, why = original_head(p)
        else:
            head, why = p["headRefOid"], "head"
        if not ensure_commit(bare, head):
            skipped.append({"pr": n, "why": f"head {head[:12]} is not fetchable"})
            continue
        e = {"pr": n, "head": head, "head_from": why, "state": p["state"]}
        if case["mode"] == "position":
            m = first_merge(p)
            if case["select"].get("merged_back") and why == "before-merge-back":
                target = m["parents"]["nodes"][1]["oid"]
            elif p["mergeCommit"]:
                target = p["mergeCommit"]["parents"]["nodes"][0]["oid"]
            else:
                skipped.append({"pr": n, "why": "no integration point"})
                continue
            if not ensure_commit(bare, target) or not is_ancestor(bare, target, main_tip):
                skipped.append({"pr": n, "why": "integration point is not on main"})
                continue
            fork = git(bare, "merge-base", head, target).out.strip()
            if not fork:
                skipped.append({"pr": n, "why": "no merge-base with main"})
                continue
            e.update(fork=fork, target=target)
        else:
            fork = git(bare, "merge-base", head, main_tip).out.strip()
            e.update(fork=fork)
        entries.append(e)
    out = {k: case[k] for k in ("id", "repo", "mode", "what")}
    out["prs"] = entries
    out["skipped"] = skipped
    if case["mode"] != "position":
        out["target"] = main_tip
    return out


def cmd_fetch(a) -> int:
    doc = {"fetched": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"), "owner": OWNER, "cases": []}
    by_repo: dict[str, dict[int, dict]] = {}
    for case in CASES:
        if a.cases and case["id"] not in a.cases:
            continue
        name = case["repo"]
        if name not in by_repo:
            by_repo[name] = {p["number"]: p for p in gh_prs(name)}
        bare = ensure_cache(a.cache, name)
        tip = rev(bare, "main")
        doc.setdefault("main", {})[name] = tip
        c = plan_case(case, by_repo[name], bare, tip)
        print(f"{case['id']}: {len(c['prs'])} PRs, {len(c['skipped'])} skipped", flush=True)
        doc["cases"].append(c)
    a.cases_file.parent.mkdir(parents=True, exist_ok=True)
    a.cases_file.write_text(json.dumps(doc, indent=2) + "\n")
    return 0


# ---------------------------------------------------------------- replay


@dataclass
class Land:
    """One tool's attempt to land one PR (or a whole stack)."""

    pr: int | str
    tool: str
    outcome: str = "error"  # ff | merged | conflict | error
    round_trips: int = 0
    rebases: int = 0
    secs: float = 0.0
    tree: str | None = None  # git tree id of the line after the land
    before: str | None = None  # git tree id of the line before it
    own: str | None = None  # git tree id of the PR's own head
    conflicts: list[str] = field(default_factory=list)
    oracle: str | None = None  # pass | broken | fail-inherited | n/a
    note: str = ""

    def doc(self) -> dict:
        return {k: v for k, v in self.__dict__.items() if v not in (None, "", [])}


def raw_delta(repo: Path, a: str, b: str) -> list[tuple[str, str, str, str]]:
    """(status, mode, blob, path) for each path that differs from a to b."""
    out = git(repo, "diff", "--raw", "-z", "--no-renames", "--no-abbrev", a, b).out
    return parse_raw(out)


def parse_raw(out: str) -> list[tuple[str, str, str, str]]:
    parts = out.split("\0")
    res, i = [], 0
    while i + 1 < len(parts):
        meta = parts[i]
        if not meta.startswith(":"):
            i += 1
            continue
        f = meta[1:].split()
        res.append((f[4][0], f[1], f[3], parts[i + 1]))
        i += 2
    return res


def apply_delta(repo: Path, delta, dest: Path) -> list[str]:
    """Writes the delta into a working copy. Returns paths it could not express (submodules)."""
    skipped = []
    for status, mode, blob, path in delta:
        p = dest / path
        if status == "D":
            if p.is_symlink() or p.exists():
                p.unlink()
            continue
        if mode == "160000":
            skipped.append(path)
            continue
        p.parent.mkdir(parents=True, exist_ok=True)
        data = subprocess.run(["git", "-C", str(repo), "cat-file", "blob", blob], capture_output=True, check=True).stdout
        if p.is_symlink() or p.exists():
            p.unlink()
        if mode == "120000":
            os.symlink(data.decode(), p)
        else:
            p.write_bytes(data)
            if mode == "100755":
                p.chmod(0o755)
    return skipped


def tree_of_dir(repo: Path, d: Path) -> str:
    """The git tree id of a directory (graft's working copy, minus .graft)."""
    with tempfile.NamedTemporaryFile(prefix="idx-", dir=repo, delete=False) as t:
        idx = t.name
    os.unlink(idx)
    try:
        env = {"GIT_INDEX_FILE": idx, "GIT_WORK_TREE": str(d)}
        git(repo, "add", "-A", "-f", "--", ".", ":(exclude).graft", env=env)
        return git(repo, "write-tree", env=env).out.strip()
    finally:
        if os.path.exists(idx):
            os.unlink(idx)


def tree(repo: Path, commit: str) -> str:
    return git(repo, "rev-parse", commit + "^{tree}").out.strip()


def parse_merge_tree(out: str) -> tuple[str, list[str]]:
    """`git merge-tree --write-tree --name-only --no-messages` output: the
    tree id, then the conflicted paths."""
    lines = [x for x in out.splitlines() if x]
    return (lines[0] if lines else ""), lines[1:]


def marker_files(wc: Path, candidates) -> list[str]:
    out = []
    for path in sorted(set(candidates)):
        p = wc / path
        if p.is_file() and not p.is_symlink():
            try:
                if b"\n<<<<<<< " in b"\n" + p.read_bytes():
                    out.append(path)
            except OSError:
                pass
        if Path(str(p) + ".graft-conflict").exists():
            out.append(path)
    return out


class Replay:
    """One case: a graft repository imported from the case's commits, a bare
    git remote, and a git working clone that shares the cache's objects."""

    def __init__(self, ctx: Ctx, case: dict, bare: Path):
        self.ctx = ctx
        self.case = case
        self.bare = bare
        self.dir = ctx.work / case["id"]
        self.dir.mkdir(parents=True, exist_ok=True)
        self.repo = f"rp-{case['id']}"
        self.gw = self.dir / "git-wc"
        self.remote = self.dir / "remote.git"
        self.lines: dict[str, str] = {}  # line -> git commit

    def line_plan(self) -> None:
        c = self.case
        for e in c["prs"]:
            self.lines[f"f-{e['pr']}"] = e["fork"]
            if c["mode"] == "position":
                self.lines[f"t-{e['pr']}"] = e["target"]
        if c["mode"] != "position":
            self.lines["main"] = c["target"]

    def setup(self) -> None:
        """Seeds both systems with the same commits."""
        self.line_plan()
        seed = self.dir / "seed.git"
        git(self.dir, "init", "-q", "--bare", seed)
        refspecs = [f"{sha}:refs/heads/{line}" for line, sha in self.lines.items()]
        for i in range(0, len(refspecs), 50):
            git(self.bare, "push", "-q", "--no-verify", seed, *refspecs[i : i + 50])
        t0 = time.perf_counter()
        args = ["import", seed, "--repo", self.repo, "--server", self.ctx.graft.url, "--db", self.dir / "mirror.db"]
        args += ["--token-file", self.ctx.graft.token_file, "--no-tags"]
        for line in self.lines:
            args += ["--line", f"refs/heads/{line}={line}"]
        run([self.ctx.mirror.bin, *args], timeout=6 * 3600)
        self.import_s = time.perf_counter() - t0
        git(self.dir, "clone", "-q", "--bare", seed, self.remote)
        git(self.dir, "clone", "-q", "--shared", "--no-checkout", self.bare, self.gw)
        git(self.gw, "remote", "add", "r", self.remote)
        git(self.gw, "config", "core.hooksPath", os.devnull)

    # -- graft

    def graft_land(self, label, line: str, fork_line: str, steps: list[tuple[str, str]]) -> Land:
        """Clones at the fork, commits each (from, to) delta as a draft, and lands."""
        g = self.ctx.graft
        o = Land(label, "graft")
        wc = self.dir / f"g-{label}"
        shutil.rmtree(wc, ignore_errors=True)
        g(["clone", self.repo, wc, "--line", line, "--at", fork_line])
        o.before = None
        touched = []
        for a, b in steps:
            delta = raw_delta(self.bare, a, b)
            touched += [d[3] for d in delta]
            skipped = apply_delta(self.bare, delta, wc)
            if skipped:
                o.note = f"submodules not replayed: {len(skipped)}"
            r = g(["commit", "-m", f"replay {label} {b[:12]}"], cwd=wc, check=False)
            if r.rc != 0 and "nothing" not in (r.err + r.out).lower():
                o.note = f"commit exit {r.rc}: {(r.err or r.out).strip()[:200]}"
                return o
        t0 = time.perf_counter()
        sync = False
        while o.round_trips < MAX_ATTEMPTS:
            o.round_trips += 1
            r, j = g.json(["land", "--sync"] if sync else ["land"], cwd=wc, check=False)
            if r.rc == 0:
                o.rebases += 1 if j.get("rebased") else 0
                o.outcome = "merged" if (j.get("rebased") or sync) else "ff"
                break
            kind = (j.get("error") or {}).get("kind")
            if r.rc == 4:
                continue
            if r.rc == 3 and kind == "conflict" and not sync:
                # The server's rebase refused: sync (the client merges by line) and land.
                o.rebases += 1
                sync = True
                continue
            if r.rc == 3:
                o.outcome = "conflict"
                o.conflicts = marker_files(wc, touched) or sorted(set(touched))[:20]
                break
            o.note = f"land exit {r.rc} ({kind}): {(r.err or r.out).strip()[:200]}"
            break
        o.secs = time.perf_counter() - t0
        if o.outcome in ("ff", "merged"):
            tip = self.dir / f"g-{label}-tip"
            shutil.rmtree(tip, ignore_errors=True)
            g(["clone", self.repo, tip, "--line", line])
            o.tree = tree_of_dir(self.gw, tip)
            shutil.rmtree(tip, ignore_errors=True)
        shutil.rmtree(wc, ignore_errors=True)
        return o

    # -- git

    def git_land(self, label, line: str, commits: list[str], onto_first: str) -> Land:
        """Pushes a chain of commits; on a rejection fetches, classifies with
        merge-tree, rebases and pushes again."""
        o = Land(label, "git")
        t0 = time.perf_counter()
        head = commits[-1]
        o.round_trips += 1
        r = git(self.gw, "push", "-q", "--no-verify", "r", f"{head}:refs/heads/{line}", check=False)
        if r.rc == 0:
            o.outcome = "ff"
            o.secs = time.perf_counter() - t0
            o.tree = tree(self.gw, head)
            return o
        o.round_trips += 1
        git(self.gw, "fetch", "-q", "r", f"+refs/heads/{line}:refs/remotes/r/{line}")
        upstream = f"refs/remotes/r/{line}"
        mt = git(self.gw, "merge-tree", "--write-tree", "--name-only", "--no-messages", upstream, head, check=False)
        _, files = parse_merge_tree(mt.out)
        o.rebases += 1
        git(self.gw, "checkout", "-q", "--detach", "-f", head)
        rb = git(self.gw, "rebase", "-q", "--onto", upstream, onto_first, check=False)
        if rb.rc != 0:
            st = git(self.gw, "diff", "--name-only", "--diff-filter=U", check=False).out.split()
            git(self.gw, "rebase", "--abort", check=False)
            o.outcome = "conflict"
            o.conflicts = sorted(set(st or files))
            o.secs = time.perf_counter() - t0
            return o
        new = rev(self.gw, "HEAD")
        o.round_trips += 1
        git(self.gw, "push", "-q", "--no-verify", "r", f"{new}:refs/heads/{line}")
        o.outcome = "merged"
        o.secs = time.perf_counter() - t0
        o.tree = tree(self.gw, new)
        if mt.rc == 1 and not files:
            o.note = "merge-tree conflict, rebase clean"
        return o

    def squash(self, base: str, head: str, label) -> str:
        """One commit with head's tree on base (graft lands one draft per PR)."""
        env = {"GIT_AUTHOR_DATE": "@0 +0000", "GIT_COMMITTER_DATE": "@0 +0000"}
        return git(self.gw, "commit-tree", tree(self.gw, head), "-p", base, "-m", f"replay {label}", env=env).out.strip()

    # -- the case

    def replay(self) -> list[Land]:
        c = self.case
        lands: list[Land] = []
        log = self.ctx.log
        if c["mode"] == "stack":
            prs = c["prs"]
            fork = prs[0]["fork"]
            heads = [e["head"] for e in prs]
            steps = list(zip([fork] + heads[:-1], heads))
            before = tree(self.gw, c["target"])
            g = self.graft_land("stack", "main", f"f-{prs[0]['pr']}", steps)
            g.before, g.own = before, tree(self.gw, heads[-1])
            lands.append(g)
            # git: one PR at a time; each lands onto main and the next is
            # rebased over it (the squash-merge flow of a stacked chain).
            prev = fork
            for e in prs:
                cm = self.squash(prev, e["head"], e["pr"])
                bt = self.remote_tree("main")
                o = self.git_land(e["pr"], "main", [cm], prev)
                o.before, o.own = bt, tree(self.gw, e["head"])
                lands.append(o)
                log(f"  git #{e['pr']}: {o.outcome}")
                if o.outcome == "conflict":
                    break
                prev = e["head"]
            return lands
        for e in c["prs"]:
            n = e["pr"]
            line = f"t-{n}" if c["mode"] == "position" else "main"
            before = self.remote_tree(line)
            gl = self.graft_land(n, line, f"f-{n}", [(e["fork"], e["head"])])
            cm = self.squash(e["fork"], e["head"], n)
            ol = self.git_land(n, line, [cm], e["fork"])
            for o in (gl, ol):
                o.before, o.own = before, tree(self.gw, e["head"])
            if c["mode"] == "sequence" and gl.tree and ol.tree and gl.tree != ol.tree:
                gl.note = (gl.note + " line diverged from git's").strip()
            lands += [gl, ol]
            log(f"  #{n}: graft {gl.outcome} ({gl.round_trips} rt), git {ol.outcome} ({ol.round_trips} rt)")
        return lands

    def remote_tree(self, line: str) -> str:
        return git(self.remote, "rev-parse", f"refs/heads/{line}^{{tree}}").out.strip()


class Oracle:
    """`cargo check` on a tree, cached by tree id, in one worktree per repository."""

    def __init__(self, ctx: Ctx, name: str, gw: Path, enabled: bool):
        self.enabled = enabled
        self.gw = gw
        self.wt = ctx.work / f"check-{name}"
        self.target = ctx.work / f"target-{name}"
        self.cache_file = ctx.work / f"oracle-{name}.json"
        self.cache: dict[str, dict] = json.loads(self.cache_file.read_text()) if self.cache_file.exists() else {}
        self.log = ctx.log
        self.secs = 0.0

    def check(self, tree_id: str) -> dict:
        if tree_id in self.cache:
            return self.cache[tree_id]
        cm = git(
            self.gw, "commit-tree", tree_id, "-m", "oracle", env={"GIT_AUTHOR_DATE": "@0 +0000", "GIT_COMMITTER_DATE": "@0 +0000"}
        ).out.strip()
        if not self.wt.exists():
            git(self.gw, "worktree", "add", "-q", "--detach", self.wt, cm)
        git(self.wt, "checkout", "-q", "-f", "--detach", cm)
        git(self.wt, "clean", "-q", "-fdx", "-e", "target")
        env = dict(os.environ, CARGO_BUILD_JOBS="4", CARGO_TARGET_DIR=str(self.target), CARGO_TERM_COLOR="never")
        t0 = time.perf_counter()
        try:
            p = subprocess.run(
                ["cargo", "check", "--workspace", "--all-targets", "--message-format=short"],
                cwd=self.wt,
                env=env,
                capture_output=True,
                timeout=3600,
            )
            rc, err = p.returncode, p.stderr.decode(errors="replace")
        except subprocess.TimeoutExpired:
            rc, err = -1, "timeout"
        secs = time.perf_counter() - t0
        self.secs += secs
        r = {"ok": rc == 0, "secs": round(secs, 1), "errors": error_summary(err) if rc else []}
        self.log(f"    cargo check {tree_id[:10]}: {'ok' if rc == 0 else 'FAIL'} ({secs:.0f}s)")
        self.cache[tree_id] = r
        self.cache_file.write_text(json.dumps(self.cache))
        return r

    def judge(self, o: Land) -> None:
        """Sets o.oracle for a land that merged (a fast-forward is the PR's own tree)."""
        if not self.enabled or o.outcome != "merged" or not o.tree:
            return
        if self.check(o.tree)["ok"]:
            o.oracle = "pass"
            return
        inputs_ok = all(self.check(t)["ok"] for t in (o.own, o.before) if t)
        o.oracle = "broken" if inputs_ok else "fail-inherited"


ERR_RE = re.compile(r"^(\S+?):\d+:\d+: error(\[E\d+\])?")


def error_summary(stderr: str, limit: int = 5) -> list[str]:
    """`file: error[E0425]` per failing location; no source text or messages."""
    out = []
    for line in stderr.splitlines():
        m = ERR_RE.match(line)
        if m:
            s = f"{m.group(1)}: error{m.group(2) or ''}"
            if s not in out:
                out.append(s)
        elif line.startswith("error: failed to") or line.startswith("error: no matching package"):
            out.append(line[:120])
        if len(out) >= limit:
            break
    return out


@dataclass
class Ctx:
    graft: Graft
    mirror: Mirror
    work: Path
    log: object


# ---------------------------------------------------------------- summary


def tally(lands: list[Land], tool: str) -> dict:
    ls = [x for x in lands if x.tool == tool]
    secs = [x.secs for x in ls]
    return {
        "lands": len(ls),
        "ff": sum(x.outcome == "ff" for x in ls),
        "merged_clean": sum(x.outcome == "merged" for x in ls),
        "conflicts": sum(x.outcome == "conflict" for x in ls),
        "errors": sum(x.outcome == "error" for x in ls),
        "broken": sum(x.oracle == "broken" for x in ls),
        "oracle_pass": sum(x.oracle == "pass" for x in ls),
        "oracle_inherited": sum(x.oracle == "fail-inherited" for x in ls),
        "rebases": sum(x.rebases for x in ls),
        "round_trips": sum(x.round_trips for x in ls),
        "wall_s": round(sum(secs), 3),
        "p50_ms": round(percentile(secs, 50) * 1000, 1) if secs else None,
    }


def agreement(lands: list[Land]) -> dict:
    """Per PR, graft against git (position and sequence modes)."""
    by: dict = {}
    for x in lands:
        by.setdefault(x.pr, {})[x.tool] = x
    out = {"both_clean": 0, "graft_only_clean": 0, "git_only_clean": 0, "both_conflict": 0, "same_tree": 0, "different_tree": 0}
    for pair in by.values():
        g, t = pair.get("graft"), pair.get("git")
        if not g or not t:
            continue
        gc, tc = g.outcome in ("ff", "merged"), t.outcome in ("ff", "merged")
        key = "both_clean" if gc and tc else "graft_only_clean" if gc else "git_only_clean" if tc else "both_conflict"
        out[key] += 1
        if gc and tc:
            out["same_tree" if g.tree == t.tree else "different_tree"] += 1
    return out


def case_md(c: dict) -> list[str]:
    out = [f"## {c['id']} ({c['mode']})", "", c["what"] + ".", ""]
    if "error" in c:
        return out + ["```", c["error"], "```", ""]
    out.append(f"{c['n_prs']} PRs replayed, {len(c.get('skipped', []))} skipped; graft import {c.get('import_s', 0):.1f} s.")
    out += ["", "| | graft | git |", "| --- | ---: | ---: |"]
    g, t = c["graft"], c["git"]
    rows = [
        ("lands attempted", "lands"),
        ("fast-forward (no merge needed)", "ff"),
        ("merged clean", "merged_clean"),
        ("conflicts", "conflicts"),
        ("clean but broken (`cargo check`)", "broken"),
        ("merged, check passed", "oracle_pass"),
        ("merged, check failed but an input failed too", "oracle_inherited"),
        ("errors", "errors"),
        ("rebases", "rebases"),
        ("client round trips", "round_trips"),
        ("integration wall time (s)", "wall_s"),
        ("p50 per land (ms)", "p50_ms"),
    ]
    for label, k in rows:
        out.append(f"| {label} | {g.get(k)} | {t.get(k)} |")
    if c.get("agreement"):
        a = c["agreement"]
        out += [
            "",
            f"Per PR: both clean {a['both_clean']} (same tree {a['same_tree']}, different {a['different_tree']}), "
            f"graft only {a['graft_only_clean']}, git only {a['git_only_clean']}, both conflict {a['both_conflict']}.",
        ]
    conf = [x for x in c["lands"] if x.get("outcome") == "conflict"]
    if conf:
        out += ["", "| PR | tool | conflicted paths |", "| --- | --- | --- |"]
        for x in conf:
            out.append(f"| {x['pr']} | {x['tool']} | {', '.join(x.get('conflicts', [])[:6])} |")
    broken = [x for x in c["lands"] if x.get("oracle") == "broken"]
    if broken:
        out += ["", "Clean but broken: " + ", ".join(f"#{x['pr']} ({x['tool']})" for x in broken) + "."]
    return out + [""]


def markdown(doc: dict) -> str:
    out = [f"# Real-PR replay: {doc['label']}", ""]
    out += [f"Started {doc['started']}, {doc['elapsed_s']} s. graft {doc.get('graft_version', '?')}, {doc.get('git_version', '?')}."]
    out += [f"Oracle: {'`cargo check --workspace --all-targets`, CARGO_BUILD_JOBS=4' if doc['oracle'] else 'off'}.", ""]
    out += [
        "| case | mode | PRs | graft clean | graft conflicts | git clean | git conflicts | graft broken | git broken | graft rt | git rt |"
    ]
    out += ["| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for c in doc["cases"]:
        if "error" in c:
            out.append(f"| {c['id']} | {c['mode']} | error | | | | | | | | |")
            continue
        g, t = c["graft"], c["git"]
        out.append(
            f"| {c['id']} | {c['mode']} | {c['n_prs']} | {g['ff'] + g['merged_clean']} | {g['conflicts']} | "
            f"{t['ff'] + t['merged_clean']} | {t['conflicts']} | {g['broken']} | {t['broken']} | {g['round_trips']} | {t['round_trips']} |"
        )
    out.append("")
    for c in doc["cases"]:
        out += case_md(c)
    return "\n".join(out)


# ---------------------------------------------------------------- run


def cmd_run(a) -> int:
    plan = json.loads(a.cases_file.read_text())
    cases = [c for c in plan["cases"] if not a.cases or c["id"] in a.cases]
    started = datetime.datetime.now(datetime.timezone.utc)
    out = a.out or BENCH / "results" / f"{started:%Y-%m-%d}-real-prs"
    out.mkdir(parents=True, exist_ok=True)
    work = a.work or Path(tempfile.mkdtemp(prefix="graft-realprs-"))
    work.mkdir(parents=True, exist_ok=True)

    def log(msg: str) -> None:
        print(f"{time.strftime('%H:%M:%S')} {msg}", flush=True)

    site = LocalSite(a.server_bin, work / "server", [f"rp-{c['id']}" for c in cases])
    graft = Graft(a.graft_bin, site.url, site.token_file)
    ctx = Ctx(graft, Mirror(a.mirror_bin, site.url, site.token_file), work, log)
    doc = {
        "label": a.label or out.name,
        "started": started.isoformat(timespec="seconds"),
        "fetched": plan.get("fetched"),
        "oracle": not a.no_oracle,
        "graft_version": graft.version(),
        "git_version": Git().version(),
        "cases": [],
    }
    t0 = time.perf_counter()
    rc = 0
    oracles: dict[str, Oracle] = {}
    try:
        for c in cases:
            log(f"[{c['id']}] {len(c['prs'])} PRs ({c['mode']})")
            r = {k: c[k] for k in ("id", "repo", "mode", "what")}
            r["skipped"] = c.get("skipped", [])
            try:
                bare = ensure_cache(a.cache, c["repo"])
                rp = Replay(ctx, c, bare)
                rp.setup()
                r["import_s"] = round(rp.import_s, 1)
                lands = rp.replay()
                if c["repo"] not in oracles:
                    oracles[c["repo"]] = Oracle(ctx, c["repo"], rp.gw, not a.no_oracle)
                for o in lands:
                    oracles[c["repo"]].judge(o)
                r["n_prs"] = len(c["prs"])
                r["graft"], r["git"] = tally(lands, "graft"), tally(lands, "git")
                if c["mode"] != "stack":
                    r["agreement"] = agreement(lands)
                r["lands"] = [x.doc() for x in lands]
            except StepError as e:
                log(f"[{c['id']}] failed: {e}")
                r["error"] = str(e)[:2000]
                rc = 2
            doc["cases"].append(r)
            _write(out, doc, time.perf_counter() - t0)
    finally:
        site.stop()
        doc["oracle_s"] = round(sum(o.secs for o in oracles.values()), 1)
        _write(out, doc, time.perf_counter() - t0)
        if not a.keep and not a.work:
            shutil.rmtree(work, ignore_errors=True)
    log(f"results: {out}")
    return rc


def _write(out: Path, doc: dict, elapsed: float) -> None:
    doc["elapsed_s"] = round(elapsed, 1)
    (out / "results.json").write_text(json.dumps(doc, indent=2) + "\n")
    (out / "summary.md").write_text(markdown(doc) + "\n")


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="graftbench.realprs", description="Replay real overlapping PRs through graft and git (WI-2338).")
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("fetch", "run"):
        s = sub.add_parser(name)
        s.add_argument("--cases-file", type=Path, default=CASES_FILE)
        s.add_argument(
            "--cache", type=Path, default=Path.home() / ".cache" / "graft-bench" / "realprs", help="bare clones of the source repositories"
        )
        s.add_argument("--case", dest="cases", action="append", help="only this case id (repeatable)")
    r = sub.choices["run"]
    bins = ROOT / "target" / "release"
    r.add_argument("--graft-bin", type=Path, default=bins / "graft")
    r.add_argument("--server-bin", type=Path, default=bins / "graft-server")
    r.add_argument("--mirror-bin", type=Path, default=bins / "graft-mirror")
    r.add_argument("--out", type=Path)
    r.add_argument("--work", type=Path)
    r.add_argument("--keep", action="store_true")
    r.add_argument("--label")
    r.add_argument("--no-oracle", action="store_true", help="skip the cargo check oracle")
    a = p.parse_args(argv)
    return cmd_fetch(a) if a.cmd == "fetch" else cmd_run(a)


if __name__ == "__main__":
    sys.exit(main())
