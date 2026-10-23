"""The graft server under test: a local one the harness starts (temp dir,
ephemeral port), or a given URL with a token file."""

from __future__ import annotations

import os
import queue
import re
import subprocess
import threading
import urllib.request
from pathlib import Path

from .proc import StepError, run

START_TIMEOUT = 30


def du(path: Path) -> int:
    """Bytes in regular files under `path`."""
    total = 0
    for dirpath, _, files in os.walk(path):
        for f in files:
            try:
                total += os.lstat(os.path.join(dirpath, f)).st_size
            except OSError:
                pass
    return total


class LocalSite:
    """A graft-server in `dir`, on a free loopback port, with the lines
    `direct` (phase 1: no checks in the land path)."""

    kind = "local"

    def __init__(self, server_bin: Path, dir: Path, repos: list[str]):
        self.bin = server_bin
        self.dir = dir
        dir.mkdir(parents=True, exist_ok=True)
        self.config = dir / "server.toml"
        self.config.write_text(
            'listen = "127.0.0.1:0"\n'
            f'objects_dir = "{dir / "objects"}"\n'
            f'meta_db = "{dir / "meta" / "meta.db"}"\n'
            "json_logs = false\n"
            "min_free_fraction = 0.0\n"
            "shutdown_grace_s = 5\n"
            "\n[default_line]\n"
            'policy = "direct"\n'
            "\n[[lines]]\n"
            'name = "main"\n'
            'policy = "direct"\n'
        )
        for repo in repos:
            self.admin(["repo", "create", repo])
        # The token goes from the child's stdout straight into a 0600 file;
        # it is never printed or passed as an argument.
        scopes = []
        for repo in repos:
            scopes += [
                "--scope",
                f"repo:{repo}:read",
                "--scope",
                f"repo:{repo}:write",
                "--scope",
                f"repo:{repo}:land:main",
                "--scope",
                f"repo:{repo}:admin",
            ]
        minted = self.admin(["token", "mint", "--subject", "graft-bench", "--kind", "agent", "--ttl", str(7 * 86400)] + scopes)
        self.token_file = dir / "bench.token"
        fd = os.open(self.token_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(minted.out.strip() + "\n")
        self.proc: subprocess.Popen | None = None
        self.url = ""
        self.start()

    def admin(self, args: list):
        return run([self.bin, "--config", self.config, "admin"] + args)

    def start(self) -> None:
        log = open(self.dir / "server.log", "ab")
        self.proc = subprocess.Popen(
            [str(self.bin), "--config", str(self.config), "serve"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=log,
        )
        found: queue.Queue = queue.Queue()

        def reader(stream):
            sent = False
            for raw in stream:
                line = raw.decode(errors="replace").strip()
                m = re.match(r"graft-server listening on (\S+)", line)
                if m and not sent:
                    found.put(m.group(1))
                    sent = True

        threading.Thread(target=reader, args=(self.proc.stdout,), daemon=True).start()
        try:
            addr = found.get(timeout=START_TIMEOUT)
        except queue.Empty:
            self.stop()
            raise StepError(f"graft-server did not listen within {START_TIMEOUT}s; see {self.dir / 'server.log'}") from None
        self.url = f"http://{addr}"

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.proc = None

    def create_repo(self, name: str) -> None:
        """Repositories are created before the server starts (above)."""

    def stats(self) -> dict:
        """Server CPU seconds and memory, from /proc, and the site's size."""
        out = {"storage_bytes": du(self.dir / "objects") + du(self.dir / "meta")}
        if self.proc and self.proc.poll() is None:
            pid = self.proc.pid
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                tick = os.sysconf("SC_CLK_TCK")
                out["cpu_s"] = round((int(fields[11]) + int(fields[12])) / tick, 3)
                for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                    if line.startswith(("VmRSS:", "VmHWM:")):
                        key = "rss_kib" if line.startswith("VmRSS") else "peak_rss_kib"
                        out[key] = int(line.split()[1])
            except (OSError, IndexError, ValueError):
                pass
        return out


class RemoteSite:
    """A running server at `url`. Its repositories must already exist and be
    empty; the token file must grant read, write, land:main and admin on them."""

    kind = "remote"

    def __init__(self, url: str, token_file: Path):
        self.url = url.rstrip("/")
        self.token_file = token_file
        if not token_file.is_file():
            raise StepError(f"token file not found: {token_file}")

    def create_repo(self, name: str) -> None:
        """Remote repositories are created by the operator (README)."""

    def stop(self) -> None:
        pass

    def stats(self) -> dict:
        """Best effort: the process metrics on `/metrics`, if it serves them."""
        out: dict = {}
        try:
            with urllib.request.urlopen(f"{self.url}/metrics", timeout=5) as r:
                text = r.read().decode(errors="replace")
        except OSError:
            return out
        for line in text.splitlines():
            if line.startswith("process_cpu_seconds_total "):
                out["cpu_s"] = float(line.split()[1])
            elif line.startswith("process_resident_memory_bytes "):
                out["rss_kib"] = int(float(line.split()[1]) / 1024)
        return out
