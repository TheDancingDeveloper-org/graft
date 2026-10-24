"""The live half of WI-2352: client sessions land real small changes on an
integration line of a graft-dev repository followed from GitHub, then
`graft-mirror pr-batch` opens one GitHub PR with one commit per land, and
one planted land is dropped with `--exclude`. Oversight reviews and merges
the PR with `land-pr` (D28); this script stops once the PR is open.

Run on the graft-dev VM, from bench/:

    python3 -m graftbench.batchlive --dir <0700 work dir> --bin <dir with graft, graft-mirror>

The work dir must hold `mirror.token`, `boot.token`, `agent-<n>.token` (agent
tokens minted with `--session` and `--work-item`) and `github.token`, all
0600. Nothing here prints a token.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import threading
import time
from pathlib import Path

from .batch import commits
from .proc import run
from .tools import Git

GUIDE = "trial/agent-guide.md"
WORK_ITEM = "WI-2352"


def edit_guide_merge(text: str) -> str:
    old = "You\ncannot land on `main`; the operator merges the finished batch on GitHub."
    new = (
        "You\ncannot land on `main`. `graft-mirror pr-batch` exports the line as one GitHub\n"
        "PR with one commit per land (D26); oversight reviews it and merges it with\n"
        "`land-pr` (D28)."
    )
    assert old in text, "guide: merge sentence not found"
    return text.replace(old, new)


def edit_guide_check(text: str) -> str:
    old = "   Landing is immediate: there is no attestation to wait for. Exit 0 means\n   your change is on `trial-1`."
    new = (
        "   Landing is immediate: there is no attestation to wait for. `g land` first\n"
        "   runs the pre-land check (D27): a dry-run rebase that stops the land on a\n"
        "   real conflict before anything is sent; `--no-check` skips it. Exit 0\n"
        "   means your change is on `trial-1`."
    )
    assert old in text, "guide: land step not found"
    return text.replace(old, new)


def edit_guide_identity(text: str) -> str:
    old = "   More edits? Commit again (a stack of drafts), or `g commit --amend`."
    new = (
        "   With `VOGT_SESSION_ID` and `VOGT_WORK_ITEM` set (Vogt sets both), the\n"
        "   draft records your session and work item and the author kind defaults to\n"
        "   `agent` (WI-2337); the batch PR's commit for your land carries them as\n"
        "   `Graft-Session` and `Graft-Work-Item` trailers.\n"
        "   More edits? Commit again (a stack of drafts), or `g commit --amend`."
    )
    assert old in text, "guide: commit step not found"
    return text.replace(old, new)


README = """# trial/

Scripts and the agent guide for graft trials that deliver through an
integration line and a batch PR (decisions D21, D26, D28).

| File | What |
| --- | --- |
| `agent-guide.md` | how an agent delivers a task through graft, not git |
| `create-trial-line.sh` | imports GitHub `main` into graft-dev and forks the trial line |
| `install-cli.sh` | builds and installs the `graft` CLI from `origin/main` |
| `mint-agent-tokens.sh` | mints one agent token per trial session on graft-dev |

Results go in `docs/trials/`.
"""

DROP_ME = """# Planted land (WI-2352)

This file is the live batch trial's planted bad land. It is dropped from the
batch with `graft-mirror pr-batch --exclude`, so it and its revert are both
in the PR's history and the file is not in the tree.
"""

# (agent, file, edit, message): agents 1-3 edit the same file.
CHANGES = [
    (1, GUIDE, edit_guide_merge, "trial guide: batch PRs are reviewed and merged by land-pr (D28)"),
    (2, GUIDE, edit_guide_check, "trial guide: graft land runs the pre-land check (D27)"),
    (3, GUIDE, edit_guide_identity, "trial guide: Vogt session and work item reach the batch commit (WI-2337)"),
    (4, "trial/README.md", lambda _t: README, "trial: add a README listing the trial scripts"),
    (5, "trial/DROP-ME.md", lambda _t: DROP_ME, "trial: planted bad land, to be dropped (WI-2352)"),
]


def main(argv=None) -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--dir", type=Path, required=True)
    p.add_argument("--bin", type=Path, required=True)
    p.add_argument("--endpoint", default="http://100.99.200.95:17888")
    p.add_argument("--repo", default="graft-wi2352")
    p.add_argument("--line", default="batch-2352")
    p.add_argument("--step", choices=["setup", "land", "cut", "drop", "analyze"], required=True)
    a = p.parse_args(argv)
    d, g, m = a.dir, a.bin / "graft", a.bin / "graft-mirror"
    state_f = d / "state.json"
    state = json.loads(state_f.read_text()) if state_f.exists() else {}

    def graft(tok, args, cwd=None, env=None, check=True):
        return run([g, "--endpoint", a.endpoint, "--token-file", d / tok] + args, cwd=cwd, env=env, check=check)

    def pr_batch(*extra):
        r = run([m, "pr-batch", a.line, "--base", "main", "--config", d / "mirror.toml"] + list(extra), check=False, timeout=900)
        return r

    def save():
        state_f.write_text(json.dumps(state, indent=2) + "\n")

    if a.step == "setup":
        (d / "mirror.toml").write_text(
            f'server = "{a.endpoint}"\nrepo = "{a.repo}"\n'
            'upstream = "https://github.com/TheDancingDeveloper-org/graft.git"\n'
            f'mirror_dir = "{d / "mirror.git"}"\ndb = "{d / "mirror.sqlite"}"\ntoken_file = "{d / "mirror.token"}"\n\n'
            '[[lines]]\nbranch = "main"\nline = "main"\n\n'
            f'[github]\nowner = "TheDancingDeveloper-org"\nrepo = "graft"\ntoken_file = "{d / "github.token"}"\n'
        )
        r = pr_batch()  # follows main; stops because the line does not exist yet
        print("first pr-batch (expected to stop):", r.rc, r.out.strip()[-300:], r.err.strip()[-300:])
        boot = d / "boot"
        if not boot.exists():
            graft("boot.token", ["clone", a.repo, boot, "--line", "main"])
        tip = json.loads(graft("boot.token", ["log", "--at", "main", "--limit", "1", "--json"], cwd=boot).out)["changesets"][0]["id"]
        graft("boot.token", ["line", "create", a.line, "--from", tip], cwd=boot)
        state["fork"] = tip
        save()
        print("line", a.line, "forked at", tip)
        return 0

    if a.step == "land":
        lands = []
        lk = threading.Lock()
        bar = threading.Barrier(len(CHANGES))

        def go(n, path, edit, msg):
            env = {
                "VOGT_SESSION_ID": f"ses_wi2352live{n}",
                "VOGT_WORK_ITEM": WORK_ITEM,
                "GRAFT_AUTHOR_NAME": f"graft trial agent {n}",
                "GRAFT_AUTHOR_EMAIL": f"trial-agent-{n}@graft.invalid",
            }
            wc = d / f"wc-{n}"
            if not wc.exists():
                graft(f"agent-{n}.token", ["clone", a.repo, wc, "--line", a.line], env=env)
            f = wc / path
            f.write_text(edit(f.read_text() if f.exists() else ""))
            graft(f"agent-{n}.token", ["commit", "-m", msg], cwd=wc, env=env)
            bar.wait()
            t0 = time.time()
            tries, syncs = 0, 0
            while True:
                tries += 1
                r = graft(f"agent-{n}.token", ["land", "--json"], cwd=wc, env=env, check=False)
                if r.rc == 3:
                    syncs += 1
                    r = graft(f"agent-{n}.token", ["land", "--sync", "--json"], cwd=wc, env=env, check=False)
                if r.rc == 0:
                    j = json.loads(r.out)
                    with lk:
                        lands.append(
                            {
                                "agent": n,
                                "id": j["landed"],
                                "path": path,
                                "message": msg,
                                "session": env["VOGT_SESSION_ID"],
                                "work_item": WORK_ITEM,
                                "name": env["GRAFT_AUTHOR_NAME"],
                                "email": env["GRAFT_AUTHOR_EMAIL"],
                                "landed_at": time.time(),
                                "land_s": round(time.time() - t0, 3),
                                "tries": tries,
                                "syncs": syncs,
                                "preflight": j.get("preflight"),
                            }
                        )
                    return
                if tries > 20:
                    with lk:
                        lands.append({"agent": n, "error": r.err[-500:]})
                    return
                time.sleep(0.2)

        ts = [threading.Thread(target=go, args=c) for c in CHANGES]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        state["lands"] = sorted(lands, key=lambda x: x.get("landed_at", 0))
        save()
        print(json.dumps(state["lands"], indent=2))
        return 0 if all("id" in x for x in lands) else 1

    def analyze(key, number):
        """The batch branch's commits on GitHub, checked against the lands."""
        git = Git()
        gh = "https://github.com/TheDancingDeveloper-org/graft.git"
        clone = d / "gh-clone"
        if not clone.exists():
            git(["init", "-q", "--bare", str(clone)])
        br = f"graft/batch-{number}"
        git(["fetch", "-q", gh, "+refs/heads/main:refs/heads/main", f"+refs/heads/{br}:refs/heads/{br}"], cwd=clone)
        base = git(["merge-base", "main", br], cwd=clone).out.strip()
        cs = commits(git, clone, f"{base}..{br}")
        by = {x["id"]: x for x in state["lands"]}
        rows = []
        for c in cs:
            land = by.get(c["trailers"].get("Graft-Changeset"))
            rows.append(
                {
                    "sha": c["sha"],
                    "subject": c["subject"],
                    "changeset": c["trailers"].get("Graft-Changeset"),
                    "author_ok": bool(land) and (c["name"], c["email"]) == (land["name"], land["email"]),
                    "session_ok": bool(land) and c["trailers"].get("Graft-Session") == land["session"],
                    "work_item_ok": bool(land) and c["trailers"].get("Graft-Work-Item") == land["work_item"],
                    "is_land": bool(land),
                }
            )
        state[key].update(branch=br, base=base, head=git(["rev-parse", br], cwd=clone).out.strip(), commits=rows)

    if a.step in ("cut", "drop", "analyze"):
        if a.step != "analyze":
            extra = []
            if a.step == "drop":
                bad = next(x for x in state["lands"] if x["agent"] == 5)
                extra = ["--exclude", bad["id"], "--reason", "planted bad land (WI-2352 live trial)"]
            t0 = time.time()
            r = pr_batch(*extra)
            state[a.step] = {"at": t0, "secs": round(r.secs, 3), "rc": r.rc, "out": r.out.strip(), "err": r.err.strip()[-800:]}
            prs = re.findall(r"PR:\s+(\S+)", r.out)
            state[a.step]["pr"] = prs[-1] if prs else None
            nums = re.findall(r"batch (\d+) \(", r.out)
            state[a.step]["batch"] = int(nums[-1]) if nums else None
            save()
        for key in ("cut", "drop"):
            if state.get(key, {}).get("batch"):
                analyze(key, state[key]["batch"])
        save()
        print(json.dumps({k: state.get(k) for k in ("cut", "drop")}, indent=2))
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
