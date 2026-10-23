"""Concurrent writers: W agents land at once, in three scenarios.

- clean:    each writer appends to a different module (disjoint files);
- merge3:   every writer edits its own region of one shared file, so all but
            the first land need a server-side rebase with a line merge;
- conflict: every writer edits the same line. The losers get the conflict
            back (graft exit 3, markers in the file; git a failed rebase)
            and resolve it by one rule: the line becomes the sorted union of
            every writer's value. The final file is the same whatever the
            order, so graft and git must end byte-identical.

graft writers run `graft land` (exit 4, contended, is retried); git writers
run `git push`, and on a rejection `git fetch` + `git rebase` and push again.
"""

from __future__ import annotations

import re
import threading
import time
from pathlib import Path

from .proc import summarize
from .tools import Git, Graft

SCENARIOS = ["clean", "merge3", "conflict"]
DIR = "bench_conc"
MAX_ATTEMPTS = 200
REGION_GAP = 6  # unchanged lines between merge3 regions


def merge_file(r: int, writers: int, values: dict[int, str] | None = None) -> bytes:
    values = values or {}
    out = [f'"""Shared module for concurrent round {r}."""', ""]
    for i in range(writers):
        out += ["", f"def region_{i}():"]
        out += [f"    pad_{k} = {k}" for k in range(REGION_GAP // 2)]
        out.append(f'    value = "{values.get(i, "base")}"')
        out += [f"    tail_{k} = {k}" for k in range(REGION_GAP // 2)]
        out += ["    return value", ""]
    return ("\n".join(out) + "\n").encode()


def conflict_file(r: int, values: list[str]) -> bytes:
    return (f'"""Shared setting for concurrent round {r}."""\n\nBEFORE = 1\nVALUE = "{",".join(sorted(values))}"\nAFTER = 2\n').encode()


def clean_marker(r: int, i: int) -> bytes:
    return f"# bench clean round {r} writer {i}\n".encode()


VALUE_RE = re.compile(r'^VALUE = "([^"]*)"$')


def resolve_conflict(path: Path) -> bool:
    """Resolves the conflict scenario by rule. Returns whether the file held
    conflict markers."""
    text = path.read_text()
    if "<<<<<<<" not in text:
        return False
    out, values, inside = [], set(), False
    for line in text.splitlines():
        if line.startswith("<<<<<<<"):
            inside, values = True, set()
            continue
        if inside and line.startswith((">>>>>>>",)):
            inside = False
            out.append(f'VALUE = "{",".join(sorted(v for v in values if v))}"')
            continue
        if inside:
            m = VALUE_RE.match(line)
            if m:
                values.update(m.group(1).split(","))
            continue
        out.append(line)
    path.write_text("\n".join(out) + "\n")
    return True


class Plan:
    """Which files the scenarios touch, and the tree they must leave."""

    def __init__(self, modules: list[str], writers: int, rounds: int):
        if writers * rounds > len(modules):
            raise ValueError(f"{writers} writers x {rounds} rounds need that many modules; the project has {len(modules)}")
        step = len(modules) // (writers * rounds)
        self.clean = {(r, i): modules[(r * writers + i) * step] for r in range(rounds) for i in range(writers)}
        self.writers = writers
        self.rounds = rounds

    def setup_files(self) -> dict[str, bytes]:
        files = {}
        for r in range(self.rounds):
            files[f"{DIR}/merge_r{r}.py"] = merge_file(r, self.writers)
            files[f"{DIR}/conflict_r{r}.py"] = conflict_file(r, [])
        return files

    def expected(self, tree: dict[str, bytes]) -> None:
        """Applies every scenario's net effect to `tree`."""
        tree.update(self.setup_files())
        for (r, i), path in self.clean.items():
            tree[path] = tree[path] + clean_marker(r, i)
        for r in range(self.rounds):
            tree[f"{DIR}/merge_r{r}.py"] = merge_file(r, self.writers, {i: f"w{i}" for i in range(self.writers)})
            tree[f"{DIR}/conflict_r{r}.py"] = conflict_file(r, [f"w{i}" for i in range(self.writers)])

    def edit(self, scenario: str, r: int, i: int, wc: Path) -> str:
        """Makes writer `i`'s edit for round `r` in its working copy; returns
        the path."""
        if scenario == "clean":
            path = self.clean[(r, i)]
            p = wc / path
            p.write_bytes(p.read_bytes() + clean_marker(r, i))
            return path
        if scenario == "merge3":
            path = f"{DIR}/merge_r{r}.py"
            p = wc / path
            text = p.read_text().splitlines(keepends=True)
            # The writer's region: find its `value =` line and change it.
            at = text.index(f"def region_{i}():\n")
            for k in range(at, len(text)):
                if text[k].startswith("    value = "):
                    text[k] = f'    value = "w{i}"\n'
                    break
            p.write_text("".join(text))
            return path
        path = f"{DIR}/conflict_r{r}.py"
        p = wc / path
        lines = p.read_text().splitlines()
        cur = next(VALUE_RE.match(x).group(1) for x in lines if VALUE_RE.match(x))
        vals = [v for v in cur.split(",") if v] + [f"w{i}"]
        p.write_bytes(conflict_file(r, vals))
        return path


class Outcome:
    def __init__(self):
        self.latency = 0.0
        self.attempts = 0
        self.rebased = False
        self.conflicts = 0
        self.refused = 0
        self.surfaced_s: float | None = None
        self.landed: str | None = None
        self.error: str | None = None


def _guarded(fn, *args) -> None:
    """Runs a writer; an exception (a command timeout, a harness bug) is
    recorded as that writer's failure, never counted as a land."""
    o = args[-1]
    try:
        fn(*args)
    except Exception as e:  # noqa: BLE001 - every failure must be counted
        o.error = f"{type(e).__name__}: {e}"[:300]
        o.landed = None


def graft_writer(g: Graft, wc: Path, scenario: str, path: str, barrier: threading.Barrier, o: Outcome) -> None:
    """Lands as an agent does: `graft land`; on exit 3 from the server's
    file-level rebase, `graft land --sync` (the client syncs, which merges
    by line, and lands); on conflict markers, resolve by rule, commit, and
    land again."""
    barrier.wait()
    t0 = time.perf_counter()
    sync = False
    while o.attempts < MAX_ATTEMPTS:
        o.attempts += 1
        r, j = g.json(["land", "--sync"] if sync else ["land"], cwd=wc, check=False)
        if r.rc == 0:
            o.landed = j.get("landed")
            o.rebased = o.rebased or bool(j.get("rebased"))
            break
        if r.rc == 4:
            o.rebased = True
            continue
        kind = j.get("error", {}).get("kind")
        if r.rc == 3 and kind == "conflict":
            # The land's rebase refused a path both sides changed (or, after
            # a clean sync, another land won the race again): sync and retry.
            o.refused += 1
            o.rebased = True
            sync = True
            continue
        if r.rc == 3 and kind == "conflicted" and scenario == "conflict":
            o.conflicts += 1
            if o.surfaced_s is None:
                o.surfaced_s = time.perf_counter() - t0
            if not resolve_conflict(wc / path):
                o.error = f"exit 3 with no markers in {path}: {r.out.strip()[:300]}"
                break
            c = g(["commit", "-m", f"resolve {path}"], cwd=wc, check=False)
            if c.rc != 0:
                o.error = f"commit after resolve: exit {c.rc}: {c.err.strip()[:300]}"
                break
            continue
        o.error = f"land exit {r.rc} ({kind}): {(r.err or r.out).strip()[:300]}"
        break
    o.latency = time.perf_counter() - t0


def git_writer(git: Git, wc: Path, scenario: str, path: str, barrier: threading.Barrier, o: Outcome) -> None:
    barrier.wait()
    t0 = time.perf_counter()
    while o.attempts < MAX_ATTEMPTS:
        o.attempts += 1
        r = git(["push", "-q", "origin", "HEAD:main"], cwd=wc, check=False)
        if r.rc == 0:
            o.landed = git.head(wc)
            break
        o.rebased = True
        f = git(["fetch", "-q", "origin"], cwd=wc, check=False)
        if f.rc != 0:
            continue
        rb = git(["rebase", "-q", "origin/main"], cwd=wc, check=False)
        if rb.rc == 0:
            continue
        unmerged = git(["diff", "--name-only", "--diff-filter=U"], cwd=wc, check=False).out.split()
        if scenario == "conflict" and unmerged == [path]:
            o.conflicts += 1
            if o.surfaced_s is None:
                o.surfaced_s = time.perf_counter() - t0
            resolve_conflict(wc / path)
            git(["add", path], cwd=wc)
            cont = git(["rebase", "--continue"], cwd=wc, check=False)
            if cont.rc != 0:
                o.error = f"rebase --continue: {cont.err.strip()[:300]}"
                git(["rebase", "--abort"], cwd=wc, check=False)
                break
            continue
        git(["rebase", "--abort"], cwd=wc, check=False)
        o.error = f"rebase failed ({unmerged}): {(rb.err or rb.out).strip()[:300]}"
        break
    o.latency = time.perf_counter() - t0


class TipWatcher:
    """Polls the line's tip while the writers land, so the harness can check
    afterwards that the line never moved backwards: every tip it saw must be
    on the final first-parent history, in the order it was seen."""

    def __init__(self, g: Graft, wc: Path):
        self.g, self.wc = g, wc
        self.tips: list[str] = []
        self.polls = 0
        self._stop = threading.Event()
        self._t = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            r, j = self.g.json(["log", "--at", "main", "--limit", "1"], cwd=self.wc, check=False)
            self.polls += 1
            if r.rc == 0 and j.get("changesets"):
                tip = j["changesets"][0]["id"]
                if not self.tips or self.tips[-1] != tip:
                    self.tips.append(tip)
            self._stop.wait(0.05)

    def __enter__(self):
        self._t.start()
        return self

    def __exit__(self, *exc):
        self._stop.set()
        self._t.join()


def _sum_outcomes(outs: list[Outcome], wall: float) -> dict:
    lat = [o.latency for o in outs if o.error is None]
    surfaced = [o.surfaced_s for o in outs if o.surfaced_s is not None]
    n = len(outs)
    return {
        "lands": n,
        "landed": len(lat),
        "failed": n - len(lat),
        "errors": [o.error for o in outs if o.error][:5],
        "latency": summarize(lat),
        "wall_s": round(wall, 3),
        "throughput_lands_per_s": round(len(lat) / wall, 3) if wall else None,
        "attempts": sum(o.attempts for o in outs),
        "contended": sum(1 for o in outs if o.rebased or o.attempts > 1),
        "contention_rate": round(sum(1 for o in outs if o.rebased or o.attempts > 1) / n, 3) if n else None,
        "conflicts_surfaced": sum(o.conflicts for o in outs),
        "file_level_refusals": sum(o.refused for o in outs),
        "conflict_surface": summarize(surfaced),
    }


def run_graft(g: Graft, wcs: list[Path], plan: Plan, landed_ids: list[str]) -> dict:
    out = {}
    for scenario in SCENARIOS:
        outs, wall = [], 0.0
        for r in range(plan.rounds):
            paths = []
            for i, wc in enumerate(wcs):
                g(["sync"], cwd=wc)
                paths.append(plan.edit(scenario, r, i, wc))
                g(["commit", "-m", f"{scenario} round {r} writer {i}", "--author-kind", "agent"], cwd=wc)
            barrier = threading.Barrier(len(wcs))
            round_outs = [Outcome() for _ in wcs]
            threads = [
                threading.Thread(target=_guarded, args=(graft_writer, g, wc, scenario, paths[i], barrier, round_outs[i]))
                for i, wc in enumerate(wcs)
            ]
            t0 = time.perf_counter()
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            wall += time.perf_counter() - t0
            outs += round_outs
            landed_ids += [o.landed for o in round_outs if o.landed]
        out[scenario] = _sum_outcomes(outs, wall)
    return out


def run_git(git: Git, wcs: list[Path], plan: Plan) -> dict:
    out = {}
    for scenario in SCENARIOS:
        outs, wall = [], 0.0
        for r in range(plan.rounds):
            paths = []
            for i, wc in enumerate(wcs):
                git(["pull", "-q", "--ff-only", "origin", "main"], cwd=wc)
                paths.append(plan.edit(scenario, r, i, wc))
                git(["commit", "-qam", f"{scenario} round {r} writer {i}"], cwd=wc)
            barrier = threading.Barrier(len(wcs))
            round_outs = [Outcome() for _ in wcs]
            threads = [
                threading.Thread(target=_guarded, args=(git_writer, git, wc, scenario, paths[i], barrier, round_outs[i]))
                for i, wc in enumerate(wcs)
            ]
            t0 = time.perf_counter()
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            wall += time.perf_counter() - t0
            outs += round_outs
        out[scenario] = _sum_outcomes(outs, wall)
    return out
