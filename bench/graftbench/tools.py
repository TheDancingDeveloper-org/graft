"""The two systems under test, driven through their command lines."""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

from .gen import Commit
from .proc import Ran, StepError, run

# Git runs with no user or system config, so a signing key, a hook or an
# alias on the host cannot change the history or the timings.
GIT_ENV = {
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_EDITOR": "true",
    "GIT_AUTHOR_NAME": "graft-bench",
    "GIT_AUTHOR_EMAIL": "bench@graft.invalid",
    "GIT_COMMITTER_NAME": "graft-bench",
    "GIT_COMMITTER_EMAIL": "bench@graft.invalid",
    # The receive-pack and upload-pack that a local push or fetch starts
    # inherit these. No background gc may repack refs under a concurrent
    # fetch, and a fetch may still ask for a tip that a racing push has
    # just replaced ("upload-pack: not our ref" in the concurrent writers).
    "GIT_CONFIG_COUNT": "3",
    "GIT_CONFIG_KEY_0": "gc.auto",
    "GIT_CONFIG_VALUE_0": "0",
    "GIT_CONFIG_KEY_1": "receive.autogc",
    "GIT_CONFIG_VALUE_1": "false",
    "GIT_CONFIG_KEY_2": "uploadpack.allowAnySHA1InWant",
    "GIT_CONFIG_VALUE_2": "true",
}


def commit_env(c: Commit) -> dict:
    name, email = c.author
    when = f"@{c.timestamp} +0000"
    return {
        "GIT_AUTHOR_NAME": name,
        "GIT_AUTHOR_EMAIL": email,
        "GIT_AUTHOR_DATE": when,
        "GIT_COMMITTER_NAME": name,
        "GIT_COMMITTER_EMAIL": email,
        "GIT_COMMITTER_DATE": when,
    }


class Git:
    def __init__(self, binary: str = "git"):
        self.bin = binary

    def __call__(self, args: list, cwd: Path | None = None, env: dict | None = None, check: bool = True, **kw) -> Ran:
        e = dict(GIT_ENV)
        if env:
            e.update(env)
        return run([self.bin] + args, cwd=cwd, env=e, check=check, **kw)

    def version(self) -> str:
        return self(["--version"]).out.strip()

    def fast_import(self, bare: Path, commits) -> dict:
        """Writes `commits` into a new bare repository with `git fast-import`.
        Returns the commit count and the elapsed time."""
        self(["init", "-q", "--bare", "-b", "main", str(bare)])
        env = dict(os.environ)
        env.update(GIT_ENV)
        p = subprocess.Popen(
            [self.bin, "fast-import", "--quiet", "--done"],
            cwd=bare,
            env=env,
            stdin=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        count = 0
        w = p.stdin
        for c in commits:
            name, email = c.author
            msg = (c.message + "\n").encode()
            w.write(b"commit refs/heads/main\n")
            for who in (b"author", b"committer"):
                w.write(b"%s %s <%s> %d +0000\n" % (who, name.encode(), email.encode(), c.timestamp))
            w.write(b"data %d\n%s\n" % (len(msg), msg))
            for op in c.ops:
                if op[0] == "w":
                    w.write(b"M 100644 inline %s\ndata %d\n%s\n" % (op[1].encode(), len(op[2]), op[2]))
                elif op[0] == "d":
                    w.write(b"D %s\n" % op[1].encode())
                else:
                    w.write(b"R %s %s\n" % (op[1].encode(), op[2].encode()))
            w.write(b"\n")
            count += 1
        w.write(b"done\n")
        w.close()
        err = p.stderr.read().decode(errors="replace")
        p.stderr.close()
        if p.wait() != 0:
            raise StepError(f"git fast-import failed: {err[-2000:]}")
        # Allow partial clones (the sparse-clone baseline) from this repository.
        self(["config", "uploadpack.allowFilter", "true"], cwd=bare)
        return {"commits": count}

    def head(self, repo: Path, rev: str = "HEAD") -> str:
        return self(["rev-parse", rev], cwd=repo).out.strip()


class Graft:
    """The `graft` CLI against one server. The token goes by `--token-file`."""

    def __init__(self, binary: Path, url: str, token_file: Path):
        self.bin = binary
        self.url = url
        self.token_file = token_file

    def __call__(self, args: list, cwd: Path | None = None, check: bool = True, **kw) -> Ran:
        full = [self.bin, "--endpoint", self.url, "--token-file", self.token_file] + args
        return run(full, cwd=cwd, check=check, **kw)

    def json(self, args: list, cwd: Path | None = None, check: bool = True) -> tuple[Ran, dict]:
        r = self(args + ["--json"], cwd=cwd, check=check)
        try:
            return r, json.loads(r.out or r.err or "{}")
        except json.JSONDecodeError:
            return r, {}

    def version(self) -> str:
        return run([self.bin, "version"], check=False).out.strip()

    def log_ids(self, wc: Path, page: int = 1000) -> list[str]:
        """Every changeset id on the line, newest first, by paging the log."""
        ids: list[str] = []
        cursor = None
        while True:
            args = ["log", "--limit", str(page)]
            if cursor:
                args += ["--cursor", cursor]
            _, j = self.json(args, cwd=wc)
            ids += [c["id"] for c in j.get("changesets", [])]
            cursor = j.get("next")
            if not cursor:
                return ids


class Mirror:
    """`graft-mirror import`, for seeding a repository and the import benchmark."""

    def __init__(self, binary: Path, url: str, token_file: Path):
        self.bin = binary
        self.url = url
        self.token_file = token_file

    def import_git(self, bare: Path, repo: str, db: Path) -> Ran:
        return run(
            [self.bin, "import", bare, "--repo", repo, "--server", self.url, "--db", db, "--token-file", self.token_file],
            timeout=24 * 3600,
        )
